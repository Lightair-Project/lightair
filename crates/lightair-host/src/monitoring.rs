//! Where "Listen in headphones" plays (docs/10-audio.md, "Monitoring"): the output devices there
//! are, and the one chosen. The mixer plays to it through [`crate::host::Host::set_headphones`].

use parking_lot::Mutex;

/// The device chosen: (name, id).
static CHOSEN: Mutex<Option<(String, String)>> = Mutex::new(None);

/// Output devices to listen on: (name, id). The first is the system default ("default").
pub fn devices() -> Vec<(String, String)> {
    let host = cpal::default_host();
    let mut out = vec![("System Default".to_string(), "default".to_string())];
    if let Ok(devices) = cpal::traits::HostTrait::output_devices(&host) {
        out.extend(devices.filter_map(|d| Some((crate::devices::name_of(&d), crate::devices::id_of(&d)?))));
    }
    out
}

/// Listens on this device (by id; "default" for the system's). Returns whether it took.
pub fn set(name: &str, id: &str) -> bool {
    *CHOSEN.lock() = Some((name.to_string(), id.to_string()));
    true
}

/// The device listened on: (name, id).
pub fn current() -> (String, String) {
    CHOSEN.lock().clone().unwrap_or_else(|| ("System Default".into(), "default".into()))
}
