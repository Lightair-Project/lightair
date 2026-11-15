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

const REMOTE: &str = "Lightair remote API";

fn remote_entry() -> Option<keyring::Entry> {
    keyring::Entry::new(REMOTE, "password").ok()
}

/// The remote API's password, made (and saved) the first time it's asked for.
pub fn remote_password() -> Option<String> {
    let entry = remote_entry()?;
    if let Ok(p) = entry.get_password() {
        return Some(p);
    }
    let p = lightair_remote::new_password();
    entry.set_password(&p).ok()?;
    Some(p)
}

/// A new remote API password (clients with the old one must reconnect with this).
pub fn new_remote_password() -> Option<String> {
    let p = lightair_remote::new_password();
    remote_entry()?.set_password(&p).ok()?;
    Some(p)
}
