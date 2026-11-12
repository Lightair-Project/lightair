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
}

impl Default for Preferences {
    fn default() -> Self {
        Self { review_before_live: true, confirm_end_stream: true }
    }
}

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
        let p = Preferences { review_before_live: false, confirm_end_stream: false };
        save_to(&file, &p).unwrap();
        assert_eq!(load_from(&file), p);
        std::fs::write(&file, b"{}").unwrap();
        assert!(load_from(&file).review_before_live, "missing keys take their defaults");
        let _ = std::fs::remove_dir_all(dir);
    }
}
