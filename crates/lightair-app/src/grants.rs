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
