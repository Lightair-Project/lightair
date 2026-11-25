//! Pictures other apps share on this computer (docs/13, "Built-in replacements"): Syphon on a
//! Mac. The servers there are, and a source's settings for one (the compositor's provider
//! receives them: `lightair-preview`'s `provide_syphon`).

use serde_json::{Value, json};

/// The source kind that shows a shared picture here.
pub const KIND: &str = "syphon-input";

/// A source's settings for the picture `id` (`name`: what [`senders`] called it).
pub fn settings(id: &str, _name: &str) -> Value {
    json!({ "uuid": id, "allow_transparency": true })
}

/// The pictures being shared now: (what to call it, its id for [`settings`]).
#[cfg(target_os = "macos")]
pub fn senders() -> Vec<(String, String)> {
    mac::senders()
}

#[cfg(not(target_os = "macos"))]
pub fn senders() -> Vec<(String, String)> {
    Vec::new()
}

#[cfg(target_os = "macos")]
pub use mac::{framework, ns_string, string_at};

#[cfg(target_os = "macos")]
mod mac {
    use std::ptr::NonNull;
    use std::sync::OnceLock;

    use objc2::msg_send;
    use objc2::rc::autoreleasepool;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2_core_foundation::{CFRetained, CFString};

    /// Syphon's classes are there (loading its framework from the app bundle if they aren't yet).
    pub fn framework() -> bool {
        static FOUND: OnceLock<bool> = OnceLock::new();
        *FOUND.get_or_init(|| {
            if AnyClass::get(c"SyphonClientBase").is_some() {
                return true;
            }
            // Lightair.app/Contents/MacOS/<exe> → Lightair.app/Contents/Frameworks
            let path = std::env::current_exe()
                .ok()
                .and_then(|exe| Some(exe.parent()?.parent()?.join("Frameworks/Syphon.framework/Syphon")));
            if let Some(path) = path.filter(|p| p.exists()) {
                // SAFETY: Syphon's framework only registers its classes on load.
                match unsafe { libloading::Library::new(&path) } {
                    // kept loaded for the rest of the run: its classes stay registered
                    Ok(lib) => std::mem::forget(lib),
                    Err(e) => tracing::warn!(%e, "Syphon: couldn't load Syphon's framework"),
                }
            }
            let found =
                AnyClass::get(c"SyphonClientBase").is_some() && AnyClass::get(c"SyphonServerDirectory").is_some();
            if !found {
                tracing::info!("Syphon: no Syphon framework");
            }
            found
        })
    }

    /// `NSString` for `s` (toll-free bridged with `CFString`).
    pub fn ns_string(s: &str) -> CFRetained<CFString> {
        CFString::from_str(s)
    }

    /// The string `dict[key]`, when it's one.
    ///
    /// # Safety
    ///
    /// `dict` must be an `NSDictionary`.
    pub unsafe fn string_at(dict: *mut AnyObject, key: &CFString) -> String {
        let key = key as *const CFString as *const AnyObject;
        // SAFETY: `dict` is a dictionary; the key is a string.
        let value: *mut AnyObject = unsafe { msg_send![dict, objectForKey: key] };
        let Some(value) = NonNull::new(value) else { return String::new() };
        // SAFETY: Syphon's descriptions keep strings under these keys (NSString is a CFString).
        let is_string: Bool = unsafe { msg_send![value.as_ptr(), isKindOfClass: AnyClass::get(c"NSString")] };
        if !is_string.as_bool() {
            return String::new();
        }
        unsafe { &*(value.as_ptr() as *const CFString) }.to_string()
    }

    /// The Syphon servers running now: ("[app] name", its UUID).
    pub fn senders() -> Vec<(String, String)> {
        let Some(directory) = framework().then(|| AnyClass::get(c"SyphonServerDirectory")).flatten() else {
            return Vec::new();
        };
        let (uuid_key, name_key, app_key) = (
            ns_string("SyphonServerDescriptionUUIDKey"),
            ns_string("SyphonServerDescriptionNameKey"),
            ns_string("SyphonServerDescriptionAppNameKey"),
        );
        autoreleasepool(|_| unsafe {
            let directory: *mut AnyObject = msg_send![directory, sharedDirectory];
            let servers: *mut AnyObject = msg_send![directory, servers];
            if servers.is_null() {
                return Vec::new();
            }
            let count: usize = msg_send![servers, count];
            (0..count)
                .filter_map(|i| {
                    let d: *mut AnyObject = msg_send![servers, objectAtIndex: i];
                    let uuid = string_at(d, &uuid_key);
                    let (app, name) = (string_at(d, &app_key), string_at(d, &name_key));
                    let label = match (app.is_empty(), name.is_empty()) {
                        (false, false) => format!("[{app}] {name}"),
                        (false, true) => app,
                        _ => name,
                    };
                    (!uuid.is_empty()).then_some((label, uuid))
                })
                .collect()
        })
    }
}
