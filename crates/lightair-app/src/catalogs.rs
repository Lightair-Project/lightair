//! Catalogs (plugins.md §3.5–3.7, catalog-formats.md): Lightair's own, when this build has its
//! root, and community catalogs the user added. Each is kept as a `CatalogState` (what's trusted)
//! plus its verified shards, under `<data>/Catalogs/`. They're fetched off the UI thread at launch
//! and every 6 hours; installing downloads the attested file from its GitHub Release, checks it
//! against the attestation, and installs it through the store. Revoked lists are enforced over
//! what's installed each time they change, and at launch.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use lightair_packages::catalog::{
    Attestation, CatalogState, Envelope, Listing, RevocationQuery, Shard, Target, TimestampOutcome, now_unix,
};
use lightair_packages::{Installed, Sha256, Source};
use semver::Version;
use serde::{Deserialize, Serialize};

/// Lightair's catalog.
pub const LIGHTAIR_URL: &str = "https://packages.lightair.app/catalog/v1/";

/// Lightair's root, when this build has one (`crates/lightair-app/catalog/root-1.json`).
#[cfg(lightair_catalog_root)]
const LIGHTAIR_ROOT: Option<&[u8]> = Some(include_bytes!("../catalog/root-1.json"));
#[cfg(not(lightair_catalog_root))]
const LIGHTAIR_ROOT: Option<&[u8]> = None;

/// How often catalogs are checked.
const EVERY: Duration = Duration::from_secs(6 * 3600);
/// The largest metadata file fetched (a shard, the snapshot).
const META_LIMIT: u64 = 16 << 20;

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    url: String,
    #[serde(default)]
    lightair: bool,
}

/// One catalog as loaded.
struct Loaded {
    entry: Entry,
    state: Option<CatalogState>,
    shards: BTreeMap<String, Shard>,
    /// The last fetch's problem, as a sentence.
    error: Option<String>,
    /// When it was last fetched without a problem (seconds since 1970).
    fetched: Option<u64>,
}

/// What the panel shows about a catalog.
#[derive(Clone, Debug)]
pub struct CatalogInfo {
    pub url: String,
    pub name: String,
    pub lightair: bool,
    pub fingerprint: String,
    pub error: Option<String>,
    pub fetched: Option<u64>,
    /// Its list hasn't expired.
    pub current: bool,
    pub packages: usize,
}

/// A package a catalog offers.
#[derive(Clone, Debug)]
pub struct Offer {
    pub catalog_url: String,
    pub catalog_name: String,
    pub lightair: bool,
    pub id: String,
    pub listing: Listing,
}

/// A newer version of an installed package, in the catalog it came from.
#[derive(Clone, Debug)]
pub struct Update {
    pub id: String,
    pub name: String,
    pub installed: Version,
    pub newer: String,
    pub catalog_url: String,
    pub catalog_name: String,
    /// What the newer version asks for that the installed one didn't.
    pub added_permissions: Vec<String>,
}

static CATALOGS: Mutex<Vec<Loaded>> = Mutex::new(Vec::new());
static GENERATION: AtomicU64 = AtomicU64::new(1);
static STARTED: AtomicBool = AtomicBool::new(false);
static FETCHING: AtomicBool = AtomicBool::new(false);

fn lock() -> std::sync::MutexGuard<'static, Vec<Loaded>> {
    CATALOGS.lock().unwrap_or_else(|e| e.into_inner())
}

fn bump() {
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Changes whenever a catalog changes (the panel reads again).
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// Whether a fetch is running.
pub fn fetching() -> bool {
    FETCHING.load(Ordering::Relaxed)
}

fn dir() -> PathBuf {
    lightair_platform::paths::data_dir().join("Catalogs")
}

/// A catalog's folder: its host and a checksum of its address.
fn folder(url: &str) -> PathBuf {
    let host: String = url
        .split_once("://")
        .map_or(url, |(_, r)| r)
        .chars()
        .take_while(|c| *c != '/')
        .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '_' })
        .collect();
    dir().join(format!("{host}-{}", Sha256::of(url.as_bytes()).short(12)))
}

fn normalise(url: &str) -> String {
    let mut u = url.trim().to_string();
    if !u.ends_with('/') {
        u.push('/');
    }
    u
}

/// Lightair's catalog address and root: the development catalog in debug builds when
/// `LIGHTAIR_CATALOG_URL` and `LIGHTAIR_CATALOG_ROOT` (a root file) are set, else the root
/// compiled in.
fn lightair_root() -> Option<(String, Vec<u8>)> {
    if cfg!(debug_assertions)
        && let (Ok(url), Ok(root)) = (std::env::var("LIGHTAIR_CATALOG_URL"), std::env::var("LIGHTAIR_CATALOG_ROOT"))
        && let Ok(bytes) = std::fs::read(&root)
    {
        return Some((normalise(&url), bytes));
    }
    LIGHTAIR_ROOT.map(|b| (LIGHTAIR_URL.to_string(), b.to_vec()))
}

/// Whether this build has Lightair's catalog.
pub fn has_lightair() -> bool {
    lightair_root().is_some()
}

fn read_entries() -> Vec<Entry> {
    std::fs::read(dir().join("catalogs.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

fn write_entries(list: &[Loaded]) {
    let entries: Vec<&Entry> = list.iter().filter(|l| !l.entry.lightair).map(|l| &l.entry).collect();
    let _ = std::fs::create_dir_all(dir());
    if let Ok(text) = serde_json::to_vec_pretty(&entries) {
        atomic_write(&dir().join("catalogs.json"), &text);
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) {
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

fn load_one(entry: Entry, fresh_root: Option<&[u8]>) -> Loaded {
    let base = folder(&entry.url);
    let now = now_unix();
    let mut state: Option<CatalogState> =
        std::fs::read(base.join("state.json")).ok().and_then(|b| serde_json::from_slice(&b).ok());
    if state.is_none()
        && let Some(root) = fresh_root
    {
        state = CatalogState::new(&entry.url, root, entry.lightair, now).ok();
    }
    let mut shards = BTreeMap::new();
    if let Some(s) = &state {
        for k in s.snapshot.iter().flat_map(|snap| snap.shards.keys()) {
            if let Ok(bytes) = std::fs::read(base.join("shards").join(format!("{k}.json")))
                && let Ok(shard) = s.check_shard(k, &bytes)
            {
                shards.insert(k.clone(), shard);
            }
        }
    }
    let error = state.is_none().then(|| "This catalog's keys couldn't be read".to_string());
    Loaded { entry, state, shards, error, fetched: None }
}

/// Loads the catalogs and starts checking them (at once, then every 6 hours). Called once at
/// launch.
pub fn start() {
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    let _ = std::thread::Builder::new().name("catalogs".into()).spawn(|| {
        {
            let mut list = Vec::new();
            if let Some((url, root)) = lightair_root() {
                list.push(load_one(Entry { url, lightair: true }, Some(&root)));
            }
            for e in read_entries() {
                if !list.iter().any(|l: &Loaded| l.entry.url == e.url) {
                    list.push(load_one(e, None));
                }
            }
            *lock() = list;
            bump();
        }
        enforce();
        loop {
            refresh_all();
            std::thread::sleep(EVERY);
        }
    });
}

/// Checks every catalog now (off the UI thread).
pub fn refresh_now() {
    let _ = std::thread::Builder::new().name("catalogs refresh".into()).spawn(refresh_all);
}

fn refresh_all() {
    if FETCHING.swap(true, Ordering::Relaxed) {
        return;
    }
    bump();
    let urls: Vec<String> = lock().iter().map(|l| l.entry.url.clone()).collect();
    let mut revoked_changed = false;
    for url in urls {
        let state = lock().iter().find(|l| l.entry.url == url).and_then(|l| l.state.clone());
        let Some(state) = state else { continue };
        let result = fetch(state);
        let mut list = lock();
        let Some(l) = list.iter_mut().find(|l| l.entry.url == url) else { continue };
        match result {
            Ok((state, shards, revoked)) => {
                let base = folder(&url);
                if let Ok(text) = serde_json::to_vec(&state) {
                    atomic_write(&base.join("state.json"), &text);
                }
                for (k, (bytes, shard)) in shards {
                    atomic_write(&base.join("shards").join(format!("{k}.json")), &bytes);
                    l.shards.insert(k, shard);
                }
                l.state = Some(state);
                l.error = None;
                l.fetched = Some(now_unix());
                revoked_changed |= revoked;
            }
            Err(e) => {
                tracing::warn!(catalog = %url, "{e}");
                l.error = Some(e);
            }
        }
    }
    FETCHING.store(false, Ordering::Relaxed);
    bump();
    if revoked_changed {
        enforce();
    }
}

type Fetched = (CatalogState, Vec<(String, (Vec<u8>, Shard))>, bool);

/// One catalog's update: newer roots, the timestamp, and when it changed the snapshot, the changed
/// shards and the revoked list. Works on a copy, kept only when everything checked.
fn fetch(mut state: CatalogState) -> Result<Fetched, String> {
    let agent = lightair_live::net::agent();
    let now = now_unix();
    for _ in 0..8 {
        let path = state.next_root_path();
        match get(&agent, &state.url(&path), META_LIMIT)? {
            Some(bytes) => state.apply_root(&bytes, now).map_err(|e| keys_changed(&e.to_string()))?,
            None => break,
        }
    }
    if state.root_expired(now) {
        return Err("This catalog's keys expired: no updates from it until it publishes new ones".into());
    }
    let ts = get(&agent, &state.url("timestamp.json"), META_LIMIT)?.ok_or("The catalog has no timestamp.json")?;
    let outcome = state.apply_timestamp(&ts, now).map_err(|e| keys_changed(&e.to_string()))?;
    let mut shards = Vec::new();
    let mut revoked = false;
    if let TimestampOutcome::NewSnapshot(_) = outcome {
        let snap = get(&agent, &state.url("snapshot.json"), META_LIMIT)?.ok_or("The catalog has no snapshot.json")?;
        let changes = state.apply_snapshot(&snap, now).map_err(|e| e.to_string())?;
        for k in changes.shards {
            let bytes = get(&agent, &state.url(&format!("shards/{k}.json")), META_LIMIT)?
                .ok_or_else(|| format!("The catalog's shard {k} is missing"))?;
            let shard = state.check_shard(&k, &bytes).map_err(|e| e.to_string())?;
            shards.push((k, (bytes, shard)));
        }
        if changes.revoked {
            let bytes =
                get(&agent, &state.url("revoked.json"), META_LIMIT)?.ok_or("The catalog's revoked list is missing")?;
            state.apply_revoked(&bytes).map_err(|e| e.to_string())?;
            revoked = true;
        }
    }
    Ok((state, shards, revoked))
}

/// A signature problem on a catalog that verified before means its keys changed.
fn keys_changed(e: &str) -> String {
    if e.contains("isn't signed by its keys") {
        "This catalog's keys changed, so updates from it stopped. If you trust it, remove it and add it again"
            .to_string()
    } else {
        e.to_string()
    }
}

/// GETs `url` (at most `limit` bytes); `None` when it isn't there (404).
fn get(agent: &ureq::Agent, url: &str, limit: u64) -> Result<Option<Vec<u8>>, String> {
    if let Some(path) = url.strip_prefix("file://") {
        return match std::fs::read(path) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("{url}: {e}")),
        };
    }
    let mut res = agent.get(url).call().map_err(|e| format!("Couldn't reach the catalog ({e})"))?;
    match res.status().as_u16() {
        404 => Ok(None),
        200..=299 => res
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map(Some)
            .map_err(|e| format!("The catalog's answer couldn't be read ({e})")),
        code => Err(format!("The catalog answered HTTP {code} for {}", url.rsplit('/').next().unwrap_or(url))),
    }
}

// ------------------------------------------------------------ what the panel reads

/// The catalogs, Lightair's first.
pub fn catalogs() -> Vec<CatalogInfo> {
    let now = now_unix();
    lock()
        .iter()
        .map(|l| CatalogInfo {
            url: l.entry.url.clone(),
            name: l.state.as_ref().map_or_else(|| l.entry.url.clone(), |s| s.name().to_string()),
            lightair: l.entry.lightair,
            fingerprint: l.state.as_ref().map(CatalogState::fingerprint).unwrap_or_default(),
            error: l.error.clone(),
            fetched: l.fetched,
            current: l.state.as_ref().is_some_and(|s| s.is_current(now)),
            packages: l.shards.values().map(|s| s.packages.len()).sum(),
        })
        .collect()
}

/// Every package the catalogs offer (yanked and gone versions are still listed in `listing`).
pub fn offers() -> Vec<Offer> {
    let list = lock();
    let mut out = Vec::new();
    for l in list.iter() {
        let name = l.state.as_ref().map_or_else(|| l.entry.url.clone(), |s| s.name().to_string());
        for shard in l.shards.values() {
            for (id, listing) in &shard.packages {
                out.push(Offer {
                    catalog_url: l.entry.url.clone(),
                    catalog_name: name.clone(),
                    lightair: l.entry.lightair,
                    id: id.clone(),
                    listing: listing.clone(),
                });
            }
        }
    }
    out.sort_by_key(|a| (!a.lightair, a.listing.name.to_lowercase()));
    out
}

/// The catalog a package was installed from, if any (its base URL in the install record).
pub fn catalog_of(installed: &Installed) -> Option<String> {
    match &installed.record.source {
        Source::Catalog { catalog, .. } if !catalog.is_empty() => Some(catalog.clone()),
        _ => None,
    }
}

/// A catalog's name by its address.
pub fn name_of(url: &str) -> Option<String> {
    lock().iter().find(|l| l.entry.url == url).and_then(|l| l.state.as_ref().map(|s| s.name().to_string()))
}

/// Whether `url` is Lightair's catalog.
pub fn is_lightair(url: &str) -> bool {
    lock().iter().any(|l| l.entry.url == url && l.entry.lightair)
}

/// The newest listed version of `id` in `catalog` that isn't yanked or gone.
fn newest(listing: &Listing) -> Option<(String, Version)> {
    listing
        .versions
        .iter()
        .filter(|(_, v)| !v.yanked && !v.gone)
        .filter_map(|(k, _)| Version::parse(k).ok().map(|v| (k.clone(), v)))
        .max_by(|a, b| a.1.cmp(&b.1))
}

/// Newer versions of installed packages, in the catalogs they came from.
pub fn updates() -> Vec<Update> {
    let installed = crate::packages::by_package();
    let list = lock();
    let mut out = Vec::new();
    for versions in installed {
        let p = &versions[0];
        let Some(catalog) = catalog_of(p) else { continue };
        let Some(l) = list.iter().find(|l| l.entry.url == catalog) else { continue };
        let Some(state) = &l.state else { continue };
        let id = p.id().as_str().to_string();
        let Some(listing) = l.shards.values().find_map(|s| s.packages.get(&id)) else { continue };
        let Some((text, v)) = newest(listing) else { continue };
        if v <= *p.version() {
            continue;
        }
        let Some(att) =
            listing.versions.get(&text).and_then(|lv| state.verify_attestation(&lv.attestation).ok())
        else {
            continue;
        };
        let had: BTreeSet<&String> = p.manifest.permissions.iter().collect();
        let added = att.permissions.iter().filter(|q| !had.contains(q)).cloned().collect();
        out.push(Update {
            id,
            name: listing.name.clone(),
            installed: p.version().clone(),
            newer: text,
            catalog_url: catalog.clone(),
            catalog_name: state.name().to_string(),
            added_permissions: added,
        });
    }
    out
}

// ------------------------------------------------------------ installing

/// Reads a version from a catalog for the install screen: verifies its attestation, downloads
/// the file for this computer from its release, and checks the file against the attestation.
/// `version`: `None` for the newest (not yanked or gone). Runs off the UI thread.
pub fn prepare(catalog_url: &str, id: &str, version: Option<&str>) -> Result<crate::packages::Prepared, String> {
    let (state, listing, catalog_name, lightair) = {
        let list = lock();
        let l = list.iter().find(|l| l.entry.url == catalog_url).ok_or("That catalog isn't added")?;
        let state = l.state.clone().ok_or("That catalog's keys couldn't be read")?;
        let listing = l
            .shards
            .values()
            .find_map(|s| s.packages.get(id))
            .cloned()
            .ok_or_else(|| format!("{id} isn't in {}", state.name()))?;
        let name = state.name().to_string();
        (state, listing, name, l.entry.lightair)
    };
    let text = match version {
        Some(v) => v.to_string(),
        None => newest(&listing).map(|(t, _)| t).ok_or("Every version of it was withdrawn")?,
    };
    let listed = listing.versions.get(&text).ok_or_else(|| format!("{} {text} isn't in the catalog", listing.name))?;
    if listed.gone {
        return Err(format!("{} {text} is gone: its author removed the release", listing.name));
    }
    let att = state.verify_attestation(&listed.attestation).map_err(|e| e.to_string())?;
    let file = att.file_for(Target::current()).ok_or("It isn't made for this computer")?.clone();
    if let Some(reason) = revoked_by_any(&att.id, &att.version, Some(&att.publisher), Some(&file.archive_sha256), &file.content_sha256, Some(catalog_url)) {
        return Err(format!("It was withdrawn: {reason}"));
    }
    let store = crate::packages::store().ok_or("the packages folder can't be made")?;
    let download = store.root().join(format!(
        ".staging-download-{}.airpkg",
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos())
    ));
    let result = download_to(&file.url, file.size, &download).and_then(|()| {
        let package = crate::packages::open_package(&download)?;
        att.check_package(&file, &package).map_err(|problems| problems.join("; "))?;
        Ok(package)
    });
    let package = match result {
        Ok(p) => p,
        Err(e) => {
            let _ = std::fs::remove_file(&download);
            return Err(e);
        }
    };
    let origin = if lightair {
        crate::packages::Origin::Lightair { reviewed: att.reviewed.is_some() }
    } else {
        crate::packages::Origin::Community { name: catalog_name }
    };
    let source = Source::Catalog { url: file.url.clone(), catalog: catalog_url.to_string(), attestation: Some(listed.attestation.clone()) };
    Ok(crate::packages::Prepared::from_download(download, package, origin, source))
}

/// Asks the window to install `id` from a catalog (`None`: Lightair's, else whichever lists it;
/// `version` `None`: the newest), through the install screen, as a `lightair://install` link
/// would. For buttons outside the Packages panel (the co-host's "PiAir isn't installed").
pub fn request_install(id: &str, version: Option<&str>, catalog: Option<&str>) {
    crate::links::queue(crate::links::Link::Install {
        id: id.to_string(),
        version: version.map(str::to_string),
        catalog: catalog.map(str::to_string),
    });
}

/// Downloads `url` (exactly `size` bytes) to `dest`.
fn download_to(url: &str, size: u64, dest: &Path) -> Result<(), String> {
    if let Some(path) = url.strip_prefix("file://") {
        std::fs::copy(path, dest).map_err(|e| format!("Couldn't copy the package: {e}"))?;
    } else {
        let agent = lightair_live::net::agent();
        let mut res = agent.get(url).call().map_err(|e| format!("Couldn't download it ({e})"))?;
        if !res.status().is_success() {
            return Err(format!("Couldn't download it: its release answered HTTP {}", res.status().as_u16()));
        }
        let mut reader = res.body_mut().with_config().limit(size + 1).reader();
        let mut out = std::fs::File::create(dest).map_err(|e| e.to_string())?;
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let n = reader.read(&mut buf).map_err(|e| format!("The download stopped ({e})"))?;
            if n == 0 {
                break;
            }
            std::io::Write::write_all(&mut out, &buf[..n]).map_err(|e| e.to_string())?;
        }
    }
    let got = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    if got != size {
        return Err(format!("The download is {got} bytes, and the catalog said {size}"));
    }
    Ok(())
}

// ------------------------------------------------------------ revocations

static REVOKED: OnceLock<Mutex<BTreeMap<(String, String), String>>> = OnceLock::new();

fn revoked_map() -> &'static Mutex<BTreeMap<(String, String), String>> {
    REVOKED.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// Why an installed version was withdrawn, when it was.
pub fn revoked_reason(id: &str, version: &Version) -> Option<String> {
    revoked_map().lock().unwrap_or_else(|e| e.into_inner()).get(&(id.to_string(), version.to_string())).cloned()
}

/// Looks a package up in every catalog's revoked list: Lightair's applies to everything by
/// checksum; a catalog's id and publisher entries, and a community list, only to its own
/// packages.
fn revoked_by_any(
    id: &str,
    version: &str,
    publisher: Option<&str>,
    archive: Option<&Sha256>,
    content: &Sha256,
    from: Option<&str>,
) -> Option<String> {
    let version = Version::parse(version).ok()?;
    let list = lock();
    for l in list.iter() {
        let Some(revoked) = l.state.as_ref().and_then(|s| s.revoked.as_ref()) else { continue };
        let q = RevocationQuery {
            id,
            version: &version,
            publisher,
            archive_sha256: archive,
            content_sha256: content,
            from_this_catalog: from == Some(l.entry.url.as_str()),
        };
        if let Some(e) = revoked.find(&q, l.entry.lightair) {
            let by = l.state.as_ref().map_or("A catalog".to_string(), |s| s.name().to_string());
            return Some(format!("{} ({by})", e.reason));
        }
    }
    None
}

/// Checks every installed package against the revoked lists; newly withdrawn ones are switched
/// off and announced.
pub fn enforce() {
    let installed = crate::packages::installed();
    let mut found = BTreeMap::new();
    for p in installed.iter() {
        let from = catalog_of(p);
        let publisher = match &p.record.source {
            Source::Catalog { attestation: Some(env), .. } => attested_publisher(env),
            _ => None,
        };
        if let Some(reason) = revoked_by_any(
            p.id().as_str(),
            &p.version().to_string(),
            publisher.as_deref(),
            p.record.archive_sha256.as_ref(),
            &p.record.content_sha256,
            from.as_deref(),
        ) {
            found.insert((p.id().as_str().to_string(), p.version().to_string()), (p.manifest.name.clone(), reason));
        }
    }
    let mut map = revoked_map().lock().unwrap_or_else(|e| e.into_inner());
    for ((id, version), (name, reason)) in &found {
        if !map.contains_key(&(id.clone(), version.clone())) {
            crate::plugin_activity::notice(name, &format!("{name} {version} was withdrawn and is switched off: {reason}"), true);
        }
    }
    *map = found.into_iter().map(|(k, (_, reason))| (k, reason)).collect();
    drop(map);
    crate::packages::changed();
}

/// The publisher an attestation names (read without verifying: it only scopes a revoked list's
/// publisher entries to packages that catalog attested).
fn attested_publisher(env: &Envelope) -> Option<String> {
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(&env.payload).ok()?;
    serde_json::from_slice::<Attestation>(&bytes).ok().map(|a| a.publisher)
}

// ------------------------------------------------------------ community catalogs

/// Fetches a community catalog's first root to show before adding it (trust on first use):
/// `(state, name, fingerprint)`. Runs off the UI thread.
pub fn read_new(url: &str) -> Result<CatalogState, String> {
    let url = normalise(url);
    if !(url.starts_with("https://") || (cfg!(debug_assertions) && (url.starts_with("http://") || url.starts_with("file://")))) {
        return Err("A catalog's address starts with https://".into());
    }
    if lock().iter().any(|l| l.entry.url == url) {
        return Err("That catalog is added already".into());
    }
    let agent = lightair_live::net::agent();
    let root = get(&agent, &format!("{url}root/1.json"), META_LIMIT)?.ok_or("There's no catalog at that address (no root/1.json)")?;
    CatalogState::new(&url, &root, false, now_unix()).map_err(|e| e.to_string())
}

/// Adds a community catalog the user accepted, and fetches it.
pub fn add(state: CatalogState) {
    let base = folder(&state.base_url);
    if let Ok(text) = serde_json::to_vec(&state) {
        atomic_write(&base.join("state.json"), &text);
    }
    {
        let mut list = lock();
        list.push(Loaded {
            entry: Entry { url: state.base_url.clone(), lightair: false },
            state: Some(state),
            shards: BTreeMap::new(),
            error: None,
            fetched: None,
        });
        write_entries(&list);
    }
    bump();
    refresh_now();
}

/// Removes a community catalog: what was installed from it stays (and gets no updates).
pub fn remove(url: &str) {
    {
        let mut list = lock();
        list.retain(|l| l.entry.lightair || l.entry.url != url);
        write_entries(&list);
    }
    let _ = std::fs::remove_dir_all(folder(url));
    bump();
}
