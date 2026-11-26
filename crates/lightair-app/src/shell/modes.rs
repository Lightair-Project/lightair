//! Workspace modes (docs/05-interface-shell.md, "Window anatomy"; SH-03): a mode is a named
//! arrangement of the panels. **Edit** is the layout the window opens with, **Live** puts Chat,
//! Activity and Outputs large beside the canvas with the mixer alone below, **Minimal** leaves
//! the canvas and its primary actions. A layout you arranged yourself is saved as a mode of its
//! own ("Save layout as a mode"). Switching only moves panels: it never touches the show.
//!
//! Modes are saved per display setup (the displays' number and sizes), since a laptop's
//! arrangement would be wrong on the studio's three screens, in `modes.json` beside the
//! preferences: they are about this computer, not the workspace.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gpui_kit::component::dock::DockAreaState;
use serde::{Deserialize, Serialize};

use super::*;

/// The modes that come with Lightair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Preset {
    Edit,
    Live,
    Minimal,
}

impl Preset {
    pub const ALL: [Preset; 3] = [Preset::Edit, Preset::Live, Preset::Minimal];

    pub fn name(self) -> &'static str {
        match self {
            Preset::Edit => "Edit",
            Preset::Live => "Live",
            Preset::Minimal => "Minimal",
        }
    }
}

/// A layout the person arranged and saved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct SavedMode {
    pub name: String,
    pub layout: DockAreaState,
}

/// What a display setup remembers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct DisplayModes {
    /// The mode in use: a preset's name or a saved mode's.
    pub current: Option<String>,
    pub custom: Vec<SavedMode>,
}

/// `modes.json`: each display setup's modes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct ModesFile {
    pub displays: BTreeMap<String, DisplayModes>,
}

pub(super) fn path() -> PathBuf {
    crate::prefs::path().with_file_name("modes.json")
}

pub(super) fn load_from(file: &Path) -> ModesFile {
    std::fs::read_to_string(file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub(super) fn save_to(file: &Path, modes: &ModesFile) -> std::io::Result<()> {
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = file.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(modes).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, file)
}

/// A key for the displays the window sits among: how many and how big ("2:1512x982+2560x1440").
pub(super) fn display_key(sizes: &[(u32, u32)]) -> String {
    let mut parts: Vec<String> = sizes.iter().map(|(w, h)| format!("{w}x{h}")).collect();
    parts.sort();
    format!("{}:{}", sizes.len(), parts.join("+"))
}

/// A name for a new saved layout that no mode has yet ("Layout 2").
pub(super) fn next_name(taken: &[String]) -> String {
    (1..).map(|n| format!("Layout {n}")).find(|n| !taken.contains(n)).expect("endless")
}

impl StudioWindow {
    fn display_key_now(&self, cx: &App) -> String {
        let sizes: Vec<(u32, u32)> = cx
            .displays()
            .iter()
            .map(|d| {
                let b = d.bounds();
                (f32::from(b.size.width) as u32, f32::from(b.size.height) as u32)
            })
            .collect();
        display_key(&sizes)
    }

    /// The mode in use on these displays (Edit until one is chosen).
    pub(super) fn current_mode(&self, cx: &App) -> String {
        load_from(&path())
            .displays
            .get(&self.display_key_now(cx))
            .and_then(|d| d.current.clone())
            .unwrap_or_else(|| Preset::Edit.name().to_string())
    }

    /// The saved layouts on these displays.
    pub(super) fn saved_modes(&self, cx: &App) -> Vec<String> {
        load_from(&path())
            .displays
            .get(&self.display_key_now(cx))
            .map(|d| d.custom.iter().map(|m| m.name.clone()).collect())
            .unwrap_or_default()
    }

    fn remember(&self, name: &str, cx: &App) {
        let key = self.display_key_now(cx);
        let mut file = load_from(&path());
        file.displays.entry(key).or_default().current = Some(name.to_string());
        let _ = save_to(&path(), &file);
    }

    /// Switches to a mode by name: a preset, or a saved layout.
    pub(super) fn switch_mode(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(dock), Some(panels)) = (self.dock.clone(), self.dock_panels.clone()) else { return };
        if let Some(preset) = Preset::ALL.into_iter().find(|p| p.name() == name) {
            dock.update(cx, |area, cx| super::docking::populate_dock(&panels, preset, area, window, cx));
        } else {
            let key = self.display_key_now(cx);
            let Some(saved) = load_from(&path()).displays.get(&key).and_then(|d| d.custom.iter().find(|m| m.name == name).cloned())
            else {
                return;
            };
            let loaded = dock.update(cx, |area, cx| area.load(saved.layout, window, cx));
            if let Err(e) = loaded {
                tracing::warn!(%e, "a saved layout didn't load");
                self.studio.update(cx, |s, _| s.issue = Some(format!("Couldn't load the layout “{name}”.").into()));
                return;
            }
        }
        self.remember(name, cx);
        self.studio.update(cx, |s, _| s.issue = Some(format!("{name} layout").into()));
        cx.notify();
    }

    /// Saves the panels as they are now as a mode of their own.
    pub(super) fn save_layout_as_mode(&mut self, cx: &mut Context<Self>) {
        let Some(dock) = self.dock.clone() else { return };
        let layout = dock.read(cx).dump(cx);
        let key = self.display_key_now(cx);
        let mut file = load_from(&path());
        let entry = file.displays.entry(key).or_default();
        let taken: Vec<String> =
            Preset::ALL.iter().map(|p| p.name().to_string()).chain(entry.custom.iter().map(|m| m.name.clone())).collect();
        let name = next_name(&taken);
        entry.custom.push(SavedMode { name: name.clone(), layout });
        entry.current = Some(name.clone());
        let _ = save_to(&path(), &file);
        self.studio.update(cx, |s, _| s.issue = Some(format!("Saved as the mode “{name}”").into()));
        cx.notify();
    }

    /// Forgets a saved layout (the presets stay).
    pub(super) fn delete_mode(&mut self, name: &str, cx: &mut Context<Self>) {
        let key = self.display_key_now(cx);
        let mut file = load_from(&path());
        if let Some(entry) = file.displays.get_mut(&key) {
            entry.custom.retain(|m| m.name != name);
            if entry.current.as_deref() == Some(name) {
                entry.current = None;
            }
        }
        let _ = save_to(&path(), &file);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{ModesFile, display_key, load_from, next_name, save_to};

    #[test]
    fn displays_are_told_apart_by_their_number_and_sizes() {
        assert_eq!(display_key(&[(1512, 982)]), "1:1512x982");
        assert_eq!(display_key(&[(2560, 1440), (1512, 982)]), display_key(&[(1512, 982), (2560, 1440)]), "their order doesn't matter");
        assert_ne!(display_key(&[(1512, 982)]), display_key(&[(1512, 982), (2560, 1440)]));
    }

    #[test]
    fn a_new_layout_gets_a_name_no_mode_has() {
        assert_eq!(next_name(&[]), "Layout 1");
        assert_eq!(next_name(&["Layout 1".into(), "Edit".into()]), "Layout 2");
        assert_eq!(next_name(&["Layout 2".into()]), "Layout 1");
    }

    #[test]
    fn modes_are_kept_per_display_setup() {
        let dir = std::env::temp_dir().join(format!("lightair-modes-{}", std::process::id()));
        let file = dir.join("modes.json");
        assert!(load_from(&file).displays.is_empty(), "no file: no modes");
        let mut modes = ModesFile::default();
        modes.displays.entry("1:1512x982".into()).or_default().current = Some("Live".into());
        save_to(&file, &modes).unwrap();
        let back = load_from(&file);
        assert_eq!(back.displays["1:1512x982"].current.as_deref(), Some("Live"));
        assert!(!back.displays.contains_key("2:1512x982+2560x1440"), "another setup starts fresh");
        std::fs::write(&file, b"nonsense").unwrap();
        assert!(load_from(&file).displays.is_empty(), "a damaged file is no modes, not a crash");
        let _ = std::fs::remove_dir_all(dir);
    }

}
