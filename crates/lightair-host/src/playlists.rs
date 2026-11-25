//! Media sources with a playlist (`playlist`: files played in turn): their files, and the
//! settings a player starts from.

use serde_json::{Value, json};

/// The files of a media source's playlist (empty: it has none).
pub fn files(settings: &Value) -> Vec<String> {
    settings["playlist"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).filter(|s| !s.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

/// The media source's settings for a playlist (its first file; never looping one file).
pub fn media_settings(settings: &Value) -> Value {
    let mut s = settings.clone();
    if let Some(first) = files(settings).first() {
        s["local_file"] = json!(first);
        s["is_local_file"] = json!(true);
        s["looping"] = json!(false);
        s["restart_on_activate"] = json!(false);
        s["close_when_inactive"] = json!(false);
    }
    s
}
