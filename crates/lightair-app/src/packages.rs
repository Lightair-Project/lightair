//! Lightair packages on this computer: the installed store (`<data dir>/Packages/`), what each
//! installed package's contents are and how the app uses them, which packages a show uses, and
//! the show's `packages.json` and `packages-lock.json`. The panel, installing and exporting are
//! in `shell/packages.rs` and `shell/package_export.rs`.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use lightair_model::Workspace;
use lightair_packages::{
    Install, Installed, Kind, MANIFEST_FILE, Package, PackageId, Project, Resolution, Source, Store, Tier,
};
use semver::Version;
use serde_json::Value;

/// The key a source's settings carry when its object came from a package whose files it doesn't
/// point at (a widget's code is copied into the show): `{"id": …, "version": …}`.
pub const MARK: &str = "lightair_package";

static STORE: OnceLock<Option<Store>> = OnceLock::new();

/// Bumped by every install and removal, so cached lists are read again.
static GENERATION: AtomicU64 = AtomicU64::new(1);

/// The installed store, opened on first use (`None` if its folder can't be made).
pub fn store() -> Option<&'static Store> {
    STORE
        .get_or_init(|| match Store::open(&lightair_platform::paths::data_dir()) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::warn!(%e, "couldn't open the packages folder");
                None
            }
        })
        .as_ref()
}

/// The store's generation: it changes whenever something is installed or removed.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

/// Says that the store changed (an install or a removal).
pub fn changed() {
    GENERATION.fetch_add(1, Ordering::AcqRel);
}

/// Every installed version, sorted by id then version; read again after a change.
pub fn installed() -> Arc<Vec<Installed>> {
    static CACHE: Mutex<Option<(u64, Arc<Vec<Installed>>)>> = Mutex::new(None);
    let now = generation();
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((g, list)) = cache.as_ref()
        && *g == now
    {
        return list.clone();
    }
    let list = Arc::new(
        store()
            .and_then(|s| s.list().map_err(|e| tracing::warn!(%e, "couldn't list packages")).ok())
            .unwrap_or_default(),
    );
    *cache = Some((now, list.clone()));
    list
}

/// The installed versions grouped by package, newest version first in each group, groups by
/// name.
pub fn by_package() -> Vec<Vec<Installed>> {
    let mut groups: BTreeMap<PackageId, Vec<Installed>> = BTreeMap::new();
    for i in installed().iter() {
        groups.entry(i.id().clone()).or_default().push(i.clone());
    }
    let mut out: Vec<Vec<Installed>> = groups
        .into_values()
        .map(|mut v| {
            v.sort_by(|a, b| b.version().cmp(a.version()));
            v
        })
        .collect();
    out.sort_by_key(|v| v[0].manifest.name.to_lowercase());
    out
}

/// Whether `path` is something to install as a package: a `.airpkg`, a `.lwidget`, or a folder
/// with a package manifest at its top.
pub fn is_package_path(path: &Path) -> bool {
    if path.is_dir() {
        return path.join(MANIFEST_FILE).is_file();
    }
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    ext == lightair_packages::ARCHIVE_EXTENSION || ext == "lwidget"
}

fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

// ------------------------------------------------------------ labels

/// "1.8 MB".
pub fn size(bytes: u64) -> String {
    crate::storage::bytes(bytes, false)
}

pub fn tier_label(tier: Option<Tier>) -> &'static str {
    match tier {
        Some(Tier::Data) => "Data",
        Some(Tier::Shader) => "Shader",
        Some(Tier::Script) => "Script",
        Some(Tier::Native) => "Native",
        None => "Unknown kind",
    }
}

/// Where a package came from, in a few words ("Neon Pack.airpkg", "Exported here").
pub fn source_label(source: &Source) -> String {
    match source {
        Source::File { name } | Source::Folder { name } | Source::Lwidget { name } => name.clone(),
        Source::Catalog { .. } => "The catalog".into(),
        Source::Git { url, rev } => format!("{url} at {rev}"),
        Source::Exported => "Exported on a computer".into(),
        Source::Unknown => "Unknown source".into(),
    }
}

/// A kind as a noun with a capital ("Widget", "Effect preset").
pub fn kind_label(kind: &Kind) -> String {
    let one = kind.count(1);
    let noun = one.trim_start_matches("1 ");
    let mut c = noun.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

pub fn kind_icon(kind: &Kind) -> &'static str {
    match kind {
        Kind::Template => "layout-dashboard",
        Kind::Look => "sparkles",
        Kind::Transition => "arrow-right-left",
        Kind::Widget => "puzzle",
        Kind::Recipe => "zap",
        Kind::EffectPreset => "sliders-horizontal",
        Kind::Theme => "palette",
        Kind::Sound => "music",
        Kind::Font => "type",
        Kind::Lut => "swatch-book",
        Kind::Avatar => "person-standing",
        Kind::Media => "image",
        Kind::ShaderEffect | Kind::ShaderTransition | Kind::ShaderSource => "wand-sparkles",
        _ => "file",
    }
}

/// "neon-lower-third" as "Neon lower third".
fn pretty(name: &str) -> String {
    let spaced = name.replace(['-', '_'], " ");
    let mut c = spaced.trim().chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

// ------------------------------------------------------------ contents

/// How the app uses one content entry.
#[derive(Clone, Debug)]
pub enum Use {
    /// A widget's folder (its `index.html`, `style.css`, `main.js`, `fields.json`): added from
    /// Add Object › Widgets or the panel.
    Widget(PathBuf),
    /// A template's `scene.json`: added as a new scene.
    Scene(PathBuf),
    /// Pictures, video and avatars: placed in the scene as objects (or dragged onto the canvas).
    Place(Vec<PathBuf>),
    /// Sound files: soundboard pads.
    Sounds(Vec<PathBuf>),
    /// A `.cube` or `.png` look-up table: a LUT effect on the selected object.
    Lut(PathBuf),
    /// An automation (`automation.json`): added to Automations, switched off.
    Recipe(PathBuf),
    /// A kind this Lightair installs but doesn't load yet (looks, effect presets, transitions,
    /// themes, fonts, shaders).
    Later,
    /// Nothing the app can read was found at its path.
    Empty,
}

/// One content entry of an installed package.
#[derive(Clone, Debug)]
pub struct Item {
    pub kind: Kind,
    pub name: String,
    pub how: Use,
}

const IMAGE: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "bmp", "tga", "heic"];
const VIDEO: &[&str] = &["mp4", "mov", "m4v", "mkv", "webm", "avi"];
const AUDIO: &[&str] = &["mp3", "wav", "m4a", "aac", "flac", "ogg", "aiff"];
const AVATAR: &[&str] = &["vrm", "glb", "inp", "inx", "uma", "lairava"];

fn ext_of(path: &Path) -> String {
    path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default()
}

fn is_live2d(path: &Path) -> bool {
    file_name(path).to_lowercase().ends_with(".model3.json")
}

/// The files at `path` (itself, or what's in the folder and the folders under it, up to a
/// few levels), by name, that `keep` accepts. A Live2D model's folder counts as its
/// `.model3.json` alone.
fn files_in(path: &Path, keep: &dyn Fn(&Path) -> bool) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: usize, keep: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut paths: Vec<PathBuf> =
            entries.flatten().map(|e| e.path()).filter(|p| !file_name(p).starts_with('.')).collect();
        paths.sort_by_key(|p| file_name(p).to_lowercase());
        // a Live2D model: its model file stands for the folder
        if let Some(model) = paths.iter().find(|p| is_live2d(p)) {
            if keep(model) {
                out.push(model.clone());
            }
            return;
        }
        for p in paths {
            if p.is_dir() {
                if depth < 4 {
                    walk(&p, depth + 1, keep, out);
                }
            } else if keep(&p) {
                out.push(p);
            }
        }
    }
    if path.is_file() {
        return if keep(path) { vec![path.to_path_buf()] } else { vec![] };
    }
    let mut out = Vec::new();
    walk(path, 0, keep, &mut out);
    out
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// A content folder's own file (`scene.json`, `automation.json`), or the path itself when it's
/// a file, or the first file in the folder with that extension.
fn main_file(path: &Path, name: &str, ext: &str) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    let named = path.join(name);
    if named.is_file() {
        return Some(named);
    }
    files_in(path, &|p| ext_of(p) == ext).into_iter().next()
}

/// What a package's contents are and how the app uses each.
pub fn items(installed: &Installed) -> Vec<Item> {
    installed
        .manifest
        .contents
        .iter()
        .map(|c| {
            let path = installed.content_path(&c.path);
            let base = c.path.rsplit('/').next().unwrap_or(&c.path);
            let mut name = pretty(
                Path::new(base).file_stem().map(|s| s.to_string_lossy().into_owned()).as_deref().unwrap_or(base),
            );
            let how = match &c.kind {
                Kind::Widget => {
                    let dir = if path.is_dir() {
                        path.clone()
                    } else {
                        path.parent().map(Path::to_path_buf).unwrap_or_default()
                    };
                    if let Some(n) =
                        read_json(&dir.join("manifest.json")).and_then(|m| m["name"].as_str().map(str::to_string))
                        && !n.trim().is_empty()
                    {
                        name = n;
                    }
                    if dir.join("index.html").is_file()
                        || !files_in(&dir, &|p| matches!(ext_of(p).as_str(), "html" | "htm" | "js")).is_empty()
                    {
                        Use::Widget(dir)
                    } else {
                        Use::Empty
                    }
                }
                Kind::Template => {
                    match main_file(&path, "scene.json", "scene").or_else(|| main_file(&path, "scene.json", "json")) {
                        Some(file) => {
                            if let Some(n) = read_json(&file).and_then(|v| v["name"].as_str().map(str::to_string))
                                && !n.trim().is_empty()
                            {
                                name = n;
                            }
                            Use::Scene(file)
                        }
                        None => Use::Empty,
                    }
                }
                Kind::Media | Kind::Avatar => {
                    let files = files_in(&path, &|p| {
                        let e = ext_of(p);
                        IMAGE.contains(&e.as_str())
                            || VIDEO.contains(&e.as_str())
                            || AUDIO.contains(&e.as_str())
                            || AVATAR.contains(&e.as_str())
                            || is_live2d(p)
                    });
                    if files.is_empty() { Use::Empty } else { Use::Place(files) }
                }
                Kind::Sound => {
                    let files = files_in(&path, &|p| AUDIO.contains(&ext_of(p).as_str()));
                    if files.is_empty() { Use::Empty } else { Use::Sounds(files) }
                }
                Kind::Lut => {
                    match files_in(&path, &|p| matches!(ext_of(p).as_str(), "cube" | "png")).into_iter().next() {
                        Some(file) => Use::Lut(file),
                        None => Use::Empty,
                    }
                }
                Kind::Recipe => match main_file(&path, "automation.json", "json") {
                    Some(file) => {
                        if let Some(n) = read_json(&file).and_then(|v| v["name"].as_str().map(str::to_string))
                            && !n.trim().is_empty()
                        {
                            name = n;
                        }
                        Use::Recipe(file)
                    }
                    None => Use::Empty,
                },
                _ => Use::Later,
            };
            Item { kind: c.kind.clone(), name, how }
        })
        .collect()
}

/// A widget from an installed package, as Add Object lists it.
#[derive(Clone, Debug)]
pub struct WidgetItem {
    pub name: &'static str,
    /// The package's name.
    pub package: &'static str,
    pub id: PackageId,
    pub version: Version,
    pub dir: PathBuf,
}

/// The same text, kept for the rest of the run (Add Object's lists take `&'static str`); each
/// different text is kept once.
fn intern(text: &str) -> &'static str {
    static SEEN: Mutex<Option<HashSet<&'static str>>> = Mutex::new(None);
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let set = seen.get_or_insert_with(HashSet::new);
    if let Some(s) = set.get(text) {
        return s;
    }
    let s: &'static str = Box::leak(text.to_string().into_boxed_str());
    set.insert(s);
    s
}

/// Every widget in the installed, usable packages (the newest version of each package).
pub fn widgets() -> Arc<Vec<WidgetItem>> {
    static CACHE: Mutex<Option<(u64, Arc<Vec<WidgetItem>>)>> = Mutex::new(None);
    let now = generation();
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((g, list)) = cache.as_ref()
        && *g == now
    {
        return list.clone();
    }
    let mut out = Vec::new();
    for versions in by_package() {
        let Some(newest) = versions.iter().find(|i| i.is_usable()) else { continue };
        for item in items(newest) {
            if let Use::Widget(dir) = item.how {
                out.push(WidgetItem {
                    name: intern(&item.name),
                    package: intern(&newest.manifest.name),
                    id: newest.id().clone(),
                    version: newest.version().clone(),
                    dir,
                });
            }
        }
    }
    let list = Arc::new(out);
    *cache = Some((now, list.clone()));
    list
}

/// The settings value that marks a source as coming from `installed`.
pub fn mark(id: &PackageId, version: &Version) -> Value {
    serde_json::json!({"id": id.as_str(), "version": version.to_string()})
}

/// A widget's size from the `manifest.json` beside its files (a `.lwidget`'s), if it says.
pub fn widget_size(dir: &Path) -> Option<(u32, u32)> {
    let m = read_json(&dir.join("manifest.json"))?;
    let s = m["size"].as_array()?;
    Some((s.first()?.as_u64()? as u32, s.get(1)?.as_u64()? as u32))
}

// ------------------------------------------------------------ installing

/// A package read for the install prompt: what it is and adds, its size and where it's from.
pub struct Prepared {
    pub path: PathBuf,
    pub name: String,
    pub id: String,
    pub version: String,
    pub summary: String,
    pub size: u64,
    pub licence: String,
    pub author: String,
    /// This version with these contents is installed already.
    pub already: bool,
    /// The package read (folders and `.airpkg`s), installed as read; `None` for a `.lwidget`.
    package: Option<Package>,
}

/// Reads what's at `path` for the install prompt; the error is a sentence to show.
pub fn prepare(path: &Path) -> Result<Prepared, String> {
    let store = store().ok_or("the packages folder can't be made")?;
    let lwidget = !path.is_dir() && ext_of(path) == "lwidget";
    if lwidget {
        // unpacked in a hidden folder of the store (cleared at the next start if left)
        let scratch = store.root().join(format!(
            ".staging-preview-{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&scratch).map_err(|e| e.to_string())?;
        let manifest = lightair_packages::lwidget::to_folder(path, &scratch, None);
        let _ = std::fs::remove_dir_all(&scratch);
        let manifest = manifest.map_err(|e| e.to_string())?;
        return Ok(Prepared {
            path: path.to_path_buf(),
            name: manifest.name.clone(),
            id: manifest.id.to_string(),
            version: manifest.version.to_string(),
            summary: manifest.summary(),
            size: std::fs::metadata(path).map_or(0, |m| m.len()),
            licence: manifest.licence.clone(),
            author: manifest.author.clone(),
            already: false,
            package: None,
        });
    }
    let package = Package::open(path).map_err(|e| e.to_string())?;
    let m = &package.manifest;
    let already =
        store.get(&m.id, &m.version).ok().flatten().is_some_and(|i| i.record.content_sha256 == package.content_sha256);
    Ok(Prepared {
        path: path.to_path_buf(),
        name: m.name.clone(),
        id: m.id.to_string(),
        version: m.version.to_string(),
        summary: m.summary(),
        size: package.size,
        licence: m.licence.clone(),
        author: m.author.clone(),
        already,
        package: Some(package),
    })
}

/// Installs what `prepare` read; the error is a sentence to show.
pub fn install(prepared: Prepared) -> Result<Install, String> {
    let store = store().ok_or("the packages folder can't be made")?;
    let result = match &prepared.package {
        Some(package) => store.install(package, Source::for_path(&prepared.path)),
        None => store.install_path(&prepared.path),
    };
    changed();
    result.map_err(|e| e.to_string())
}

/// Removes one installed version, or every version of a package (`None`).
pub fn remove(id: &PackageId, version: Option<&Version>) -> Result<(), String> {
    let store = store().ok_or("the packages folder can't be made")?;
    let result = match version {
        Some(v) => store.remove(id, v),
        None => store.remove_all(id).map(|_| ()),
    };
    changed();
    result.map_err(|e| e.to_string())
}

// ------------------------------------------------------------ shows

/// The packages something in `workspace` comes from, with the version when it's known: files
/// in the store (pictures, video, sounds, scenes' media, LUTs), and sources marked with
/// [`MARK`] (widgets).
pub fn used_by(workspace: &Workspace) -> BTreeMap<PackageId, Option<Version>> {
    let mut out = BTreeMap::new();
    let Some(store) = store() else { return out };
    let root = store.root().to_string_lossy().into_owned();
    let Ok(value) = serde_json::to_value(workspace) else { return out };
    fn walk(v: &Value, root: &str, out: &mut BTreeMap<PackageId, Option<Version>>) {
        match v {
            Value::String(s) => {
                let Some(rest) = s.strip_prefix(root) else { return };
                let mut parts = rest.split(['/', '\\']).filter(|p| !p.is_empty());
                if let Some(Ok(id)) = parts.next().map(PackageId::new) {
                    let version = parts.next().and_then(|v| Version::parse(v).ok());
                    out.entry(id).or_insert(version);
                }
            }
            Value::Array(a) => a.iter().for_each(|x| walk(x, root, out)),
            Value::Object(m) => {
                if let Some(Ok(id)) = m.get(MARK).and_then(|p| p["id"].as_str()).map(PackageId::new) {
                    let version = m[MARK]["version"].as_str().and_then(|v| Version::parse(v).ok());
                    out.entry(id).or_insert(version);
                }
                m.values().for_each(|x| walk(x, root, out));
            }
            _ => {}
        }
    }
    walk(&value, &root, &mut out);
    out
}

/// Brings the show's `packages.json` and `packages-lock.json` up to date with what it uses: each
/// package something in it comes from is needed (at the version it uses), and the lock is
/// resolved again (dependencies included). Entries nothing uses any more stay until Remove
/// unused. A show that uses no packages and never had the files gets none. Called when the
/// show is saved.
pub fn record_show(dir: &Path, workspace: &Workspace) {
    let had = dir.join(lightair_packages::PROJECT_FILE).exists();
    let used = used_by(workspace);
    if used.is_empty() && !had {
        return;
    }
    let mut project = match Project::load(dir) {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(%e, "couldn't read the show's packages");
            return;
        }
    };
    let installed = installed();
    let mut changed = false;
    for (id, version) in &used {
        let locked = project.lock.packages.get(id).map(|l| l.version.clone());
        let pick = installed
            .iter()
            .filter(|i| i.id() == id)
            .filter(|i| match (version, &locked) {
                (Some(v), _) => i.version() == v,
                (None, Some(v)) => i.version() == v,
                (None, None) => true,
            })
            .max_by(|a, b| a.version().cmp(b.version()));
        if let Some(i) = pick {
            changed |= project.add(i);
        }
    }
    let resolution = project.resolve(&installed, &Default::default());
    changed |= project.apply(&resolution);
    if (changed || !had)
        && let Err(e) = project.save()
    {
        tracing::warn!(%e, "couldn't save the show's packages");
    }
}

/// What the show in `dir` needs: its packages resolved against the store (`None` when it
/// needs none, or its files can't be read).
pub fn check_show(dir: &Path) -> Option<(Project, Resolution)> {
    let project = Project::load(dir).map_err(|e| tracing::warn!(%e, "couldn't read the show's packages")).ok()?;
    if project.requirements.packages.is_empty() {
        return None;
    }
    let resolution = project.resolve(&installed(), &Default::default());
    Some((project, resolution))
}

/// Drops the show's packages nothing in it comes from any more; returns how many went.
pub fn remove_unused(dir: &Path, workspace: &Workspace) -> Result<usize, String> {
    let mut project = Project::load(dir).map_err(|e| e.to_string())?;
    let used = used_by(workspace).into_keys().collect();
    let removed = project.remove_unused(&used, &installed());
    project.save().map_err(|e| e.to_string())?;
    Ok(removed.len())
}
