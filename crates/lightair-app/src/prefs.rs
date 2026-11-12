//! App preferences (docs/14-data-persistence-settings.md, "Settings"): how Lightair behaves on
//! this Mac, as opposed to project settings, which travel with the project.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    /// Show the Go Live review sheet before going live.
    pub review_before_live: bool,
    /// Ending a stream takes a second click.
    pub confirm_end_stream: bool,
    /// Audio sample rate (Hz); used when Lightair starts.
    pub sample_rate: u32,
    /// "Listen in headphones" device: (name, id); `None`: the system default.
    pub monitor_device: Option<(String, String)>,
    /// The loudness the LUFS readout aims for.
    pub loudness_target: i32,
    /// Defaults for newly added cameras.
    pub camera: CameraDefaults,
    /// Shortcuts that work while Lightair isn't in front.
    pub global_hotkeys: Vec<GlobalHotkey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraDefaults {
    /// Short side in pixels (720, 1080, 2160); `None`: the camera's own best.
    pub height: Option<u32>,
    /// New cameras start with Camera Studio on (its first look).
    pub studio_on: bool,
}

impl Default for CameraDefaults {
    fn default() -> Self {
        Self { height: Some(1080), studio_on: false }
    }
}

impl CameraDefaults {
    /// A new camera's settings: its device, sized by the preset for `height`.
    pub fn settings(&self, device: &str) -> serde_json::Value {
        let preset = match self.height {
            Some(720) => "AVCaptureSessionPreset1280x720",
            Some(1080) => "AVCaptureSessionPreset1920x1080",
            Some(2160) => "AVCaptureSessionPreset3840x2160",
            _ => "AVCaptureSessionPresetHigh",
        };
        serde_json::json!({"device": device, "use_preset": true, "preset": preset})
    }
}

/// A shortcut that works in any app: what it does and its keys ("cmd-shift-r").
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlobalHotkey {
    pub action: crate::hotkeys::GlobalAction,
    pub keys: String,
}

impl Default for Preferences {
    fn default() -> Self {
        Self {
            review_before_live: true,
            confirm_end_stream: true,
            sample_rate: 48_000,
            monitor_device: None,
            loudness_target: -14,
            camera: CameraDefaults::default(),
            global_hotkeys: Vec::new(),
        }
    }
}

/// The sample rate libobs started with (a change applies next launch).
pub static RUNNING_SAMPLE_RATE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(48_000);

/// The loudness target, for the mixer's readout.
pub static LOUDNESS_TARGET: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(-14);

pub fn path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join("Library/Application Support/Lightair/preferences.json")
}

/// Reads preferences, falling back to defaults for a missing or unreadable file.
pub fn load_from(path: &Path) -> Preferences {
    std::fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

/// Writes preferences atomically.
pub fn save_to(path: &Path, prefs: &Preferences) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(prefs)?)?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_defaults() {
        let dir = std::env::temp_dir().join(format!("lightair-prefs-{}", std::process::id()));
        let file = dir.join("preferences.json");
        assert_eq!(load_from(&file), Preferences::default(), "missing file → defaults");
        let p = Preferences { review_before_live: false, confirm_end_stream: false, ..Preferences::default() };
        save_to(&file, &p).unwrap();
        assert_eq!(load_from(&file), p);
        std::fs::write(&file, b"{}").unwrap();
        assert!(load_from(&file).review_before_live, "missing keys take their defaults");
        let _ = std::fs::remove_dir_all(dir);
    }
}
