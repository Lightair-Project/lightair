//! Secrets live in the OS credential store, never in workspace files
//! (docs/14-data-persistence-settings.md, "Security of stored data").

use lightair_core::Id;

const SERVICE: &str = "Lightair stream key";

fn entry(destination: Id) -> Option<keyring::Entry> {
    keyring::Entry::new(SERVICE, &destination.to_string()).ok()
}

pub fn set_stream_key(destination: Id, key: &str) -> Result<(), String> {
    entry(destination).ok_or("credential store unavailable")?.set_password(key).map_err(|e| e.to_string())
}

pub fn stream_key(destination: Id) -> Option<String> {
    entry(destination)?.get_password().ok()
}

pub fn delete_stream_key(destination: Id) {
    if let Some(e) = entry(destination) {
        let _ = e.delete_credential();
    }
}
