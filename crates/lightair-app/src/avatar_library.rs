//! The avatar library (docs/plans/avatars.md, "Library"): the avatars a person keeps, across
//! workspaces, to switch between. Each is a model file (any type Lightair loads), a name, a
//! picture of it, and its look: framing, zoom, size, mirror, how much of the body follows, and
//! its expressions' shortcuts. An avatar object using one carries its id (`library`), and its
//! look is kept in the library as it changes, so switching back brings it back as it was.
//!
//! Kept in Lightair's data folder (`avatars/library.json`, pictures beside it), not in a
//! workspace: an avatar is the person's, whatever show it's in. The model files stay where they
//! are.

use std::path::{Path, PathBuf};

use lightair_model::Id;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The settings an avatar takes with it from object to object.
pub const LOOK: [&str; 7] = ["framing", "zoom", "width", "height", "mirror", "body", "expression_keys"];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: Id,
    pub name: String,
    /// The model file.
    pub file: String,
    /// Its picture's file name in the library's folder (a new name each time it's taken, so a
    /// picture shown before isn't the one shown now).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    /// [`LOOK`]'s settings, as last used.
    #[serde(default)]
    pub look: Value,
}

impl Entry {
    pub fn new(name: &str, file: &str) -> Entry {
        Entry { id: Id::new(), name: name.into(), file: file.into(), picture: None, look: json!({}) }
    }

    pub fn picture_path(&self) -> Option<PathBuf> {
        self.picture.as_ref().map(|p| dir().join(p)).filter(|p| p.exists())
    }

    /// The model's type, as people know it.
    pub fn kind(&self) -> &'static str {
        kind_of(&self.file)
    }
}

/// A model file's type, as people know it.
pub fn kind_of(file: &str) -> &'static str {
    let f = file.to_lowercase();
    if f.ends_with(".model3.json") {
        "Live2D"
    } else if f.ends_with(".inp") || f.ends_with(".inx") {
        "Inochi2D"
    } else if f.ends_with(".uma") {
        "Umamo"
    } else if f.ends_with(".lairava") {
        "PNGtuber"
    } else {
        "VRM"
    }
}

/// A picture's size in pixels (read from its header once, then remembered).
pub fn picture_size(path: &Path) -> Option<(u32, u32)> {
    static SIZES: std::sync::Mutex<Option<std::collections::HashMap<PathBuf, (u32, u32)>>> =
        std::sync::Mutex::new(None);
    let mut sizes = SIZES.lock().ok()?;
    let sizes = sizes.get_or_insert_with(Default::default);
    if let Some(s) = sizes.get(path) {
        return Some(*s);
    }
    let s = image::image_dimensions(path).ok()?;
    sizes.insert(path.to_path_buf(), s);
    Some(s)
}

/// The size to draw a picture at inside a `w`×`h` box, keeping its shape (it fits inside,
/// centred by whatever holds it).
pub fn fit(path: &Path, w: f32, h: f32) -> (f32, f32) {
    match picture_size(path) {
        Some((pw, ph)) if pw > 0 && ph > 0 => {
            let k = (w / pw as f32).min(h / ph as f32);
            (pw as f32 * k, ph as f32 * k)
        }
        _ => (w, h),
    }
}

/// An avatar's name from its file ("mao_pro" for mao_pro.model3.json).
pub fn name_of(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().trim_end_matches(".model3").to_string())
        .unwrap_or_else(|| "Avatar".into())
}

/// `look`'s settings over `settings`.
pub fn with_look(settings: &Value, look: &Value) -> Value {
    let mut s = if settings.is_object() { settings.clone() } else { json!({}) };
    for k in LOOK {
        if let Some(v) = look.get(k) {
            s[k] = v.clone();
        }
    }
    s
}

/// The look an avatar object's settings have now.
pub fn look_of(settings: &Value) -> Value {
    let mut look = json!({});
    for k in LOOK {
        if let Some(v) = settings.get(k) {
            look[k] = v.clone();
        }
    }
    look
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Library {
    pub avatars: Vec<Entry>,
}

pub fn dir() -> PathBuf {
    lightair_platform::paths::data_dir().join("avatars")
}

impl Library {
    pub fn load() -> Library {
        std::fs::read(dir().join("library.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let d = dir();
        let write = || -> std::io::Result<()> {
            std::fs::create_dir_all(&d)?;
            let tmp = d.join("library.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
            std::fs::rename(tmp, d.join("library.json"))
        };
        if let Err(e) = write() {
            tracing::warn!(%e, "the avatar library couldn't be saved");
        }
    }

    pub fn get(&self, id: Id) -> Option<&Entry> {
        self.avatars.iter().find(|e| e.id == id)
    }

    pub fn get_mut(&mut self, id: Id) -> Option<&mut Entry> {
        self.avatars.iter_mut().find(|e| e.id == id)
    }

    /// The entry for this file, made if there isn't one.
    pub fn for_file(&mut self, path: &Path) -> Id {
        let file = path.to_string_lossy().into_owned();
        if let Some(e) = self.avatars.iter().find(|e| e.file == file) {
            return e.id;
        }
        let e = Entry::new(&name_of(path), &file);
        let id = e.id;
        self.avatars.push(e);
        id
    }

    /// A copy right after it, "<name> copy", with its own picture file.
    pub fn duplicate(&mut self, id: Id) -> Option<Id> {
        let i = self.avatars.iter().position(|e| e.id == id)?;
        let mut copy = self.avatars[i].clone();
        copy.id = Id::new();
        copy.name = format!("{} copy", copy.name);
        if let Some(from) = self.avatars[i].picture_path() {
            let name = format!("{}-{}.png", copy.id, stamp());
            if std::fs::copy(&from, dir().join(&name)).is_ok() {
                copy.picture = Some(name);
            } else {
                copy.picture = None;
            }
        }
        let new = copy.id;
        self.avatars.insert(i + 1, copy);
        Some(new)
    }

    /// Takes it out of the library (and its picture); the model file stays.
    pub fn remove(&mut self, id: Id) {
        if let Some(p) = self.get(id).and_then(Entry::picture_path) {
            let _ = std::fs::remove_file(p);
        }
        self.avatars.retain(|e| e.id != id);
    }

    /// Keeps a picture (PNG bytes) for it, in place of the one before.
    pub fn set_picture(&mut self, id: Id, png: &[u8]) {
        let Some(e) = self.get_mut(id) else { return };
        let name = format!("{id}-{}.png", stamp());
        let path = dir().join(&name);
        if std::fs::create_dir_all(dir()).and_then(|_| std::fs::write(&path, png)).is_err() {
            return;
        }
        if let Some(old) = e.picture.replace(name) {
            let _ = std::fs::remove_file(dir().join(old));
        }
    }
}

fn stamp() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis())
}
