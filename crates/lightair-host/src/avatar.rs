//! Avatar sources (docs/plans/avatars.md).

/// The avatar source's kind.
pub const KIND: &str = "lightair_avatar";

use std::collections::HashMap;

use lightair_model::Id;
use parking_lot::Mutex;

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
