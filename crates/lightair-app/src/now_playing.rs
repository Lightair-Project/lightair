//! What music is playing (docs/plans/live-integrations.md, 2.8): Spotify's or Apple Music's
//! current track, asked of the app itself (AppleScript: no developer account, no sign-in; macOS
//! asks once whether Lightair may). Only while a Now playing widget exists, and only of a music
//! app that's already open (asking never opens it).

use std::time::Duration;

use serde_json::{Value, json};

const SCRIPT: &str = r#"
if application "Spotify" is running then
  tell application "Spotify"
    if player state is playing then return "spotify" & tab & (name of current track) & tab & (artist of current track) & tab & (album of current track) & tab & (artwork url of current track)
  end tell
end if
if application "Music" is running then
  tell application "Music"
    if player state is playing then return "music" & tab & (name of current track) & tab & (artist of current track) & tab & (album of current track) & tab & ""
  end tell
end if
return ""
"#;

/// The track playing now (`null` when nothing is).
pub fn now() -> Value {
    if !cfg!(target_os = "macos") {
        return Value::Null;
    }
    let Ok(out) = std::process::Command::new("/usr/bin/osascript").arg("-e").arg(SCRIPT).output() else { return Value::Null };
    let text = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
    let parts: Vec<&str> = text.split('\t').collect();
    match parts.as_slice() {
        [app, title, artist, album, art, ..] => json!({"app": app, "title": title, "artist": artist, "album": album, "artwork": art}),
        _ => Value::Null,
    }
}

/// Keeps the widgets' "now_playing" up to date while one shows it.
pub fn start(widgets: lightair_widgets::Widgets) {
    let _ = std::thread::Builder::new().name("lightair-now-playing".into()).spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(3));
            if widgets.has_kind("now-playing") {
                widgets.set_data("now_playing", now());
            }
        }
    });
}
