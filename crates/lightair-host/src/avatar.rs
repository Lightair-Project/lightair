//! Avatar sources (docs/plans/avatars.md).

/// The avatar source's kind.
pub const KIND: &str = "lightair_avatar";

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use lightair_avatar::source::Settings;
use lightair_model::Id;
use parking_lot::Mutex;

use crate::avatar_pins::{AtRest, Spot};

/// What an avatar is doing, for the Inspector.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub loaded: bool,
    /// A face is being followed.
    pub tracking: bool,
    /// Why the model didn't load.
    pub error: String,
    /// The model's own name.
    pub name: String,
    /// What body tracking sees ("body, left arm", "no body"; empty when off).
    pub body: String,
    /// The model's expressions.
    pub expressions: Vec<String>,
    /// What things can be pinned to (bones, meshes), in the picker's order.
    pub anchors: Vec<crate::avatar_pins::Anchor>,
    /// Which of its expressions are costumes (a PNGtuber's).
    pub costumes: Vec<String>,
}

/// Each avatar's status, as whatever poses it last said (by document id).
static STATUS: Mutex<Option<HashMap<Id, Status>>> = Mutex::new(None);

/// What `source` is doing now (its posing says).
pub fn set_status(source: Id, status: Option<Status>) {
    let mut all = STATUS.lock();
    let all = all.get_or_insert_with(HashMap::new);
    match status {
        Some(s) => all.insert(source, s),
        None => all.remove(&source),
    };
}

/// An avatar source's status (the default before anything poses it).
pub fn status_of(source: &Id) -> Status {
    STATUS.lock().as_ref().and_then(|a| a.get(source).cloned()).unwrap_or_default()
}

/// A model's file and revision.
type Read = (String, i64);

/// Each model the compositor draws, at rest, by its file and revision (held by its renderer).
static RESTS: Mutex<Option<HashMap<Read, Weak<AtRest>>>> = Mutex::new(None);

/// The compositor draws `file` (read at `revision`), and this is it at rest.
pub fn set_rest(file: &str, revision: i64, rest: &Arc<AtRest>) {
    let mut all = RESTS.lock();
    let all = all.get_or_insert_with(HashMap::new);
    all.retain(|_, r| r.strong_count() > 0);
    all.insert((file.to_string(), revision), Arc::downgrade(rest));
}

/// Where anchor `key` of the avatar with these settings is in its picture at rest (pixels), once
/// the compositor draws its model. `size`: as it would be at that size of the picture instead.
pub fn anchor_rest(settings: &Settings, key: &str, size: Option<(u32, u32)>) -> Option<Spot> {
    let rest = RESTS.lock().as_ref()?.get(&(settings.file.clone(), settings.revision))?.upgrade()?;
    let (w, h) = match size {
        Some((w, h)) if w > 0 && h > 0 => (w, h),
        _ => (settings.width, settings.height),
    };
    rest.spot(key, settings.framing, settings.zoom, w as f32, h as f32)
}
