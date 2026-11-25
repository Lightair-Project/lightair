//! Secrets live in the OS credential store, never in workspace files
//! (docs/14-data-persistence-settings.md, "Security of stored data").

use lightair_model::Id;

const SERVICE: &str = "Lightair stream key";

fn entry(destination: Id) -> Option<keyring::Entry> {
    keyring::Entry::new(SERVICE, &destination.to_string()).ok()
}

/// `LIGHTAIR_NO_KEYCHAIN=1`, for runs nobody is watching (the benchmark, unattended checks): the
/// app reads and writes nothing in the Keychain, so it never asks for it. Saved secrets act as
/// absent and new ones aren't kept; the remote API's password is `LIGHTAIR_REMOTE_PASSWORD` (else
/// one made for this run). Debug builds' phone pairing (`LIGHTAIR_DEV_PAIRING`) does the same.
pub fn keychain_off() -> bool {
    dev_pairing() || std::env::var("LIGHTAIR_NO_KEYCHAIN").is_ok_and(|v| v == "1")
}

pub fn set_stream_key(destination: Id, key: &str) -> Result<(), String> {
    if keychain_off() {
        return Err("the Keychain is off for this run (LIGHTAIR_NO_KEYCHAIN)".into());
    }
    entry(destination).ok_or("credential store unavailable")?.set_password(key).map_err(|e| e.to_string())
}

pub fn stream_key(destination: Id) -> Option<String> {
    if keychain_off() {
        return None;
    }
    entry(destination)?.get_password().ok()
}

pub fn delete_stream_key(destination: Id) {
    if keychain_off() {
        return;
    }
    if let Some(e) = entry(destination) {
        let _ = e.delete_credential();
    }
}

const REMOTE: &str = "Lightair remote API";

fn remote_entry() -> Option<keyring::Entry> {
    keyring::Entry::new(REMOTE, "password").ok()
}

/// Debug builds only, with `LIGHTAIR_DEV_PAIRING=1`: phones pair with no one at the computer
/// (a simulator testing the round trip), and nothing is read from the Keychain (an unsigned
/// debug build can't read the signed app's items without asking). Release builds never do.
pub fn dev_pairing() -> bool {
    cfg!(debug_assertions) && std::env::var("LIGHTAIR_DEV_PAIRING").is_ok_and(|v| v == "1")
}

/// The remote API's password while the Keychain is off: `LIGHTAIR_REMOTE_PASSWORD`, else one made
/// for this run.
fn dev_password() -> String {
    static P: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    P.get_or_init(|| std::env::var("LIGHTAIR_REMOTE_PASSWORD").unwrap_or_else(|_| lightair_remote::new_password()))
        .clone()
}

/// The remote API's password, made (and saved) the first time it's asked for.
pub fn remote_password() -> Option<String> {
    if keychain_off() {
        return Some(dev_password());
    }
    let entry = remote_entry()?;
    if let Ok(p) = entry.get_password() {
        return Some(p);
    }
    let p = lightair_remote::new_password();
    entry.set_password(&p).ok()?;
    Some(p)
}

/// The remote API's password, read on a thread of its own as the app starts (while libobs
/// starts), so the API answers as soon as the window is up rather than a credential read later.
static EARLY: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

pub fn read_remote_password_early() {
    let _ = std::thread::Builder::new().name("lightair-secrets".into()).spawn(|| {
        let _ = EARLY.set(remote_password());
    });
}

/// What the early read found, once it has (`None`: not yet).
pub fn early_remote_password() -> Option<Option<String>> {
    EARLY.get().cloned()
}

/// A new remote API password (clients with the old one must reconnect with this).
pub fn new_remote_password() -> Option<String> {
    if keychain_off() {
        return Some(dev_password());
    }
    let p = lightair_remote::new_password();
    remote_entry()?.set_password(&p).ok()?;
    Some(p)
}

const CONNECTION: &str = "Lightair connection";

/// A connected service's token ("streamlabs", "streamelements", …).
pub fn connection_token(service: &str) -> Option<String> {
    if keychain_off() {
        return None;
    }
    keyring::Entry::new(CONNECTION, service).ok()?.get_password().ok().filter(|t| !t.is_empty())
}

pub fn set_connection_token(service: &str, token: &str) -> Result<(), String> {
    if keychain_off() {
        return Err("the Keychain is off for this run (LIGHTAIR_NO_KEYCHAIN)".into());
    }
    let entry = keyring::Entry::new(CONNECTION, service).map_err(|e| e.to_string())?;
    if token.is_empty() {
        let _ = entry.delete_credential();
        Ok(())
    } else {
        entry.set_password(token).map_err(|e| e.to_string())
    }
}
