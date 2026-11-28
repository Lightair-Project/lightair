//! What each package may do on this computer (plugins.md §4.8): every permission it declares is
//! granted when it's installed, and the user can take any of them back in the Packages panel.
//! Only what was taken back is kept (per package id, across versions), so a decision stays when
//! the package updates; a version that asks for something new asks first (the install screen).

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

fn file() -> PathBuf {
    lightair_platform::paths::data_dir().join("Package permissions.json")
}

fn taken_back() -> &'static Mutex<BTreeMap<String, BTreeSet<String>>> {
    static T: OnceLock<Mutex<BTreeMap<String, BTreeSet<String>>>> = OnceLock::new();
    T.get_or_init(|| {
        let map = std::fs::read(file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Mutex::new(map)
    })
}

fn save(map: &BTreeMap<String, BTreeSet<String>>) {
    let path = file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_vec_pretty(map) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Whether `id` may use `permission` (it declared it and it wasn't taken back).
pub fn is_granted(id: &str, permission: &str) -> bool {
    !taken_back().lock().unwrap_or_else(|e| e.into_inner()).get(id).is_some_and(|s| s.contains(permission))
}

/// The permissions taken back from `id`.
pub fn revoked(id: &str) -> BTreeSet<String> {
    taken_back().lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned().unwrap_or_default()
}

/// Takes `permission` back from `id` (`grant` again gives it back).
pub fn revoke(id: &str, permission: &str) {
    let mut map = taken_back().lock().unwrap_or_else(|e| e.into_inner());
    map.entry(id.to_string()).or_default().insert(permission.to_string());
    save(&map);
    crate::packages::changed();
}

/// Gives `permission` back to `id`.
pub fn grant(id: &str, permission: &str) {
    let mut map = taken_back().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = map.get_mut(id) {
        set.remove(permission);
        if set.is_empty() {
            map.remove(id);
        }
    }
    save(&map);
    crate::packages::changed();
}

/// Forgets every decision about `id` (when it's removed for good).
pub fn forget(id: &str) {
    let mut map = taken_back().lock().unwrap_or_else(|e| e.into_inner());
    if map.remove(id).is_some() {
        save(&map);
    }
}

// ------------------------------------------------------------- optional permissions

/// What a package may ask for while it runs (a manifest's `optional_permissions`) is never
/// granted at install: it asks (`permissions.request`), the user answers on a second screen, and
/// what they allowed is kept here per package id until it gives it back, the user takes it back in
/// the Packages panel, or the package is removed.
fn optional_file() -> PathBuf {
    lightair_platform::paths::data_dir().join("Package optional permissions.json")
}

fn optional_store() -> &'static Mutex<BTreeMap<String, BTreeSet<String>>> {
    static T: OnceLock<Mutex<BTreeMap<String, BTreeSet<String>>>> = OnceLock::new();
    T.get_or_init(|| {
        let map = std::fs::read(optional_file()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Mutex::new(map)
    })
}

fn save_optional(map: &BTreeMap<String, BTreeSet<String>>) {
    let path = optional_file();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_vec_pretty(map) {
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// The optional permissions `id` holds now.
pub fn optional_granted(id: &str) -> BTreeSet<String> {
    optional_store().lock().unwrap_or_else(|e| e.into_inner()).get(id).cloned().unwrap_or_default()
}

/// Gives `id` some of its optional permissions (the user allowed them).
pub fn grant_optional(id: &str, permissions: &[String]) {
    if permissions.is_empty() {
        return;
    }
    let mut map = optional_store().lock().unwrap_or_else(|e| e.into_inner());
    map.entry(id.to_string()).or_default().extend(permissions.iter().cloned());
    save_optional(&map);
    crate::packages::changed();
}

/// Takes optional permissions from `id` (it gave them back, or the user took them back).
pub fn release_optional(id: &str, permissions: &[String]) {
    let mut map = optional_store().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(set) = map.get_mut(id) {
        for p in permissions {
            set.remove(p);
        }
        if set.is_empty() {
            map.remove(id);
        }
        save_optional(&map);
        crate::packages::changed();
    }
}

/// Forgets every optional grant of `id` (when it's removed for good).
pub fn forget_optional(id: &str) {
    let mut map = optional_store().lock().unwrap_or_else(|e| e.into_inner());
    if map.remove(id).is_some() {
        save_optional(&map);
    }
}
