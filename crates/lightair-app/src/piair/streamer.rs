//! AI streamer mode (plugins.md §5.9): PiAir runs the stream as a full AI VTuber, deciding what
//! to do next and acting on chat's requests, within a stream policy Lightair enforces in code.
//!
//! - **Modes:** off (a co-host, as before), producer (a human is around: risky actions are
//!   confirmed as today), autonomous (nobody watching: risky actions are refused, never
//!   auto-approved).
//! - **The policy** (`<data>/PiAir/lightair.json`, Lightair's own file, outside PiAir's sandbox):
//!   the allowed apps (already there), allowed sites, the scenes chat may pick, the apps (games)
//!   chat may switch to, whether chat may browse, and the games chat may play (keys only into
//!   that game's window).
//! - **Every action** PiAir asks for in this mode is checked here and written to the action log
//!   (`<data>/PiAir/actions.jsonl`, also outside its sandbox, so it can't rewrite it).
//!
//! What PiAir says caused an action (`cause`: the streamer, chat, a guest, or itself) comes from
//! PiAir. Lightair applies chat's stricter rules to what PiAir reports as chat's; it can't check
//! the claim. The hard limits that hold whatever PiAir says are the allowed apps, the allowed
//! sites, the sandbox, the permissions it holds, the kill switch and the remote stop.

use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The optional permissions AI streamer mode asks for, released when it's switched off.
pub const MODE_PERMISSIONS: [&str; 3] = ["computer.autonomous", "outputs.control", "chat.write"];

/// The stream policy.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    /// "off", "producer" or "autonomous".
    pub mode: String,
    /// Sites PiAir may open (`example.com` covers its subdomains).
    pub sites: Vec<String>,
    /// Scene names chat may pick.
    pub scenes: Vec<String>,
    /// Apps (from the allowed apps) chat may switch to: games, a music player.
    pub games: Vec<String>,
    /// Chat may ask it to open allowed sites.
    pub chat_browse: bool,
    /// Apps where chat may play: its requests become keys and clicks in that app only.
    pub chat_plays: Vec<String>,
}

impl Policy {
    pub fn on(&self) -> bool {
        matches!(self.mode.as_str(), "producer" | "autonomous")
    }

    pub fn autonomous(&self) -> bool {
        self.mode == "autonomous"
    }

    /// Whether `url` is on an allowed site (http and https only).
    pub fn site_allowed(&self, url: &str) -> bool {
        let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) else { return false };
        let host = rest.split(['/', '?', '#', ':']).next().unwrap_or("").to_ascii_lowercase();
        !host.is_empty()
            && self.sites.iter().any(|s| {
                let s = s.trim().trim_start_matches("*.").to_ascii_lowercase();
                !s.is_empty() && (host == s || host.ends_with(&format!(".{s}")))
            })
    }
}

/// One action, as the log keeps it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub at: String,
    pub method: String,
    pub what: String,
    /// "streamer", "chat", "guest" or "self".
    pub cause: String,
    pub allowed: bool,
    pub reason: String,
}

fn log_path() -> PathBuf {
    lightair_platform::paths::data_dir().join("PiAir").join("actions.jsonl")
}

/// Writes one action to the log (kept to the last 5,000 lines).
pub fn log(method: &str, what: &str, cause: &str, allowed: bool, reason: &str) {
    let e = Entry {
        at: lightair_packages::catalog::format_rfc3339(lightair_packages::catalog::now_unix()),
        method: method.into(),
        what: what.chars().take(300).collect(),
        cause: cause.into(),
        allowed,
        reason: reason.into(),
    };
    let path = log_path();
    if let Some(d) = path.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path)
        && let Ok(line) = serde_json::to_string(&e)
    {
        let _ = writeln!(f, "{line}");
    }
    trim_log(&path);
}

fn trim_log(path: &PathBuf) {
    const KEEP: usize = 5000;
    let Ok(meta) = std::fs::metadata(path) else { return };
    if meta.len() < 2_000_000 {
        return;
    }
    if let Ok(text) = std::fs::read_to_string(path) {
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(KEEP);
        let _ = std::fs::write(path, lines[start..].join("\n") + "\n");
    }
}

/// The last `n` actions, newest last.
pub fn recent(n: usize) -> Vec<Entry> {
    let text = std::fs::read_to_string(log_path()).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].iter().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// What caused a call, as PiAir reports it (the streamer when it doesn't say).
pub fn cause_of(params: &Value) -> String {
    match params["cause"].as_str() {
        Some(c @ ("chat" | "guest" | "self" | "streamer")) => c.to_string(),
        _ => "streamer".to_string(),
    }
}

/// Methods only AI streamer mode has.
pub fn streamer_only(method: &str) -> bool {
    matches!(method, "computer.open_url" | "streamer.scene" | "streamer.app")
}

/// One of Lightair's own methods (the remote API's), as PiAir calls them by name.
fn lightair_method(m: &str) -> bool {
    !(m.starts_with("computer.")
        || m.starts_with("cli.")
        || m.starts_with("secret.")
        || m.starts_with("permissions.")
        || m.starts_with("avatar.")
        || streamer_only(m))
}

/// Lightair methods that start or stop streaming and recording (they need `outputs.control`).
fn outputs_method(m: &str) -> bool {
    ["stream.", "record.", "outputs.", "replay."].iter().any(|p| m.starts_with(p))
        || matches!(m, "StartStream" | "StopStream" | "StartRecord" | "StopRecord" | "ToggleStream" | "ToggleRecord")
}

/// The stream policy's check of one call, before Lightair's other gates (permissions, AI drives,
/// the allowed apps). `front` is the frontmost app's id, for chat plays; `holds` says whether
/// PiAir holds a permission. Every refusal is a sentence PiAir turns into a tool error.
pub fn check(
    policy: &Policy,
    method: &str,
    params: &Value,
    front: Option<&str>,
    holds: &dyn Fn(&str) -> bool,
) -> Result<(), (String, String)> {
    let cause = cause_of(params);
    let refuse = |code: &str, why: String| Err((code.to_string(), why));
    if streamer_only(method) && !policy.on() {
        return refuse("mode_off", "AI streamer mode is off.".into());
    }
    if policy.on() && (cause == "self" || streamer_only(method)) && !holds("computer.autonomous") {
        return refuse(
            "not_granted",
            "PiAir wasn't given this permission: acting on its own within your stream policy.".into(),
        );
    }
    if lightair_method(method) {
        let m = method;
        if outputs_method(m) && !holds("outputs.control") {
            return refuse(
                "not_granted",
                "PiAir wasn't given this permission: start and stop streaming and recording.".into(),
            );
        }
        if m == "chat.send" && !holds("chat.write") {
            return refuse(
                "not_granted",
                "PiAir wasn't given this permission: send messages to your stream's chat.".into(),
            );
        }
        if cause == "chat" || cause == "guest" {
            return refuse("chat_not_allowed", "Chat can't change Lightair: only pick an allowed scene.".into());
        }
    }
    match method {
        "computer.open_url" => {
            let url = params["url"].as_str().unwrap_or_default();
            if !policy.site_allowed(url) {
                return refuse(
                    "site_not_allowed",
                    "That site isn't on your stream policy's list of allowed sites.".into(),
                );
            }
            if (cause == "chat" || cause == "guest") && !policy.chat_browse {
                return refuse("chat_not_allowed", "Your stream policy doesn't let chat open sites.".into());
            }
        }
        "streamer.scene" => {
            let name = params["name"].as_str().unwrap_or_default();
            if (cause == "chat" || cause == "guest") && !policy.scenes.iter().any(|s| s.eq_ignore_ascii_case(name)) {
                return refuse(
                    "chat_not_allowed",
                    format!("Chat may only pick these scenes: {}.", policy.scenes.join(", ")),
                );
            }
        }
        "streamer.app" => {
            let app = params["app"].as_str().unwrap_or_default();
            if (cause == "chat" || cause == "guest") && !policy.games.iter().any(|g| g.eq_ignore_ascii_case(app)) {
                return refuse(
                    "chat_not_allowed",
                    "Chat may only switch to the games and apps on your stream policy.".into(),
                );
            }
        }
        m if m.starts_with("computer.")
            && !matches!(m, "computer.screenshot" | "computer.cursor")
            && (cause == "chat" || cause == "guest") =>
        {
            // chat plays: keys and clicks only into a game chat may play, while it's in front
            let plays = front.is_some_and(|f| policy.chat_plays.iter().any(|g| g.eq_ignore_ascii_case(f)));
            let input = matches!(m, "computer.key" | "computer.type" | "computer.click" | "computer.move");
            if !(plays && input) {
                return refuse(
                    "chat_not_allowed",
                    "Chat can't use the computer here (chat plays is only for the games you named).".into(),
                );
            }
        }
        _ => {}
    }
    Ok(())
}

/// Opens an allowed site in the system's browser.
pub fn open_url(url: &str) -> Result<Value, (String, String)> {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("/usr/bin/open").arg(url).spawn();
    #[cfg(windows)]
    let r = std::process::Command::new("rundll32").args(["url.dll,FileProtocolHandler", url]).spawn();
    #[cfg(not(any(target_os = "macos", windows)))]
    let r: std::io::Result<std::process::Child> = Err(std::io::Error::other("not on this platform"));
    r.map(|_| json!({})).map_err(|e| ("failed".to_string(), e.to_string()))
}

/// Brings an app (by its bundle id on macOS, its executable name on Windows) to the front,
/// starting it if needed.
pub fn bring_app(app: &str) -> Result<Value, (String, String)> {
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("/usr/bin/open").args(["-b", app]).spawn();
    #[cfg(windows)]
    let r = std::process::Command::new("cmd").args(["/C", "start", "", app]).spawn();
    #[cfg(not(any(target_os = "macos", windows)))]
    let r: std::io::Result<std::process::Child> = Err(std::io::Error::other("not on this platform"));
    r.map(|_| json!({})).map_err(|e| ("failed".to_string(), e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(mode: &str) -> Policy {
        Policy {
            mode: mode.into(),
            sites: vec!["youtube.com".into()],
            scenes: vec!["Gameplay".into(), "Just chatting".into()],
            games: vec!["com.example.racer".into()],
            chat_browse: true,
            chat_plays: vec!["com.example.racer".into()],
        }
    }

    /// The policy's answer when PiAir holds everything AI streamer mode asks for.
    fn decide(p: &Policy, method: &str, params: Value, front: Option<&str>) -> Result<(), String> {
        check(p, method, &params, front, &|_| true).map_err(|(code, _)| code)
    }

    #[test]
    fn chat_may_pick_a_scene_on_the_list_and_nothing_else() {
        let p = policy("autonomous");
        assert_eq!(decide(&p, "streamer.scene", json!({"name": "gameplay", "cause": "chat"}), None), Ok(()));
        assert_eq!(
            decide(&p, "streamer.scene", json!({"name": "Desktop", "cause": "chat"}), None),
            Err("chat_not_allowed".into())
        );
    }

    #[test]
    fn a_site_off_the_list_is_refused_whoever_asks() {
        let p = policy("autonomous");
        for cause in ["chat", "self", "streamer"] {
            let r = decide(&p, "computer.open_url", json!({"url": "https://bank.example/login", "cause": cause}), None);
            assert_eq!(r, Err("site_not_allowed".into()), "{cause}");
        }
        assert_eq!(
            decide(&p, "computer.open_url", json!({"url": "https://www.youtube.com/watch", "cause": "chat"}), None),
            Ok(())
        );
    }

    #[test]
    fn chat_cant_open_even_an_allowed_site_when_browsing_is_off() {
        let p = Policy { chat_browse: false, ..policy("autonomous") };
        let r = decide(&p, "computer.open_url", json!({"url": "https://youtube.com/", "cause": "chat"}), None);
        assert_eq!(r, Err("chat_not_allowed".into()));
    }

    #[test]
    fn chat_plays_sends_keys_only_into_the_named_game_while_it_is_in_front() {
        let p = policy("autonomous");
        let key = json!({"keys": "w", "cause": "chat"});
        assert_eq!(decide(&p, "computer.key", key.clone(), Some("com.example.racer")), Ok(()));
        assert_eq!(decide(&p, "computer.key", key.clone(), Some("com.apple.Safari")), Err("chat_not_allowed".into()));
        assert_eq!(decide(&p, "computer.key", key, None), Err("chat_not_allowed".into()));
        let drag = json!({"cause": "chat"});
        assert_eq!(decide(&p, "computer.drag", drag, Some("com.example.racer")), Err("chat_not_allowed".into()));
    }

    #[test]
    fn chat_cant_change_lightair_itself() {
        let p = policy("autonomous");
        assert_eq!(decide(&p, "scene.remove", json!({"cause": "chat"}), None), Err("chat_not_allowed".into()));
        assert_eq!(decide(&p, "scene.remove", json!({"cause": "guest"}), None), Err("chat_not_allowed".into()));
    }

    #[test]
    fn streaming_and_recording_need_outputs_control() {
        let p = policy("autonomous");
        let holds_none_of_outputs = |perm: &str| perm != "outputs.control";
        let r = check(&p, "stream.start", &json!({"cause": "self"}), None, &holds_none_of_outputs);
        assert_eq!(r.map_err(|(c, _)| c), Err("not_granted".into()));
        assert_eq!(decide(&p, "stream.start", json!({"cause": "self"}), None), Ok(()));
    }

    #[test]
    fn acting_on_its_own_needs_computer_autonomous() {
        let p = policy("autonomous");
        let without = |perm: &str| perm != "computer.autonomous";
        let r = check(&p, "streamer.scene", &json!({"name": "Gameplay", "cause": "self"}), None, &without);
        assert_eq!(r.map_err(|(c, _)| c), Err("not_granted".into()));
    }

    #[test]
    fn the_modes_own_actions_are_refused_while_the_mode_is_off() {
        let p = policy("off");
        let r = decide(&p, "streamer.scene", json!({"name": "Gameplay", "cause": "streamer"}), None);
        assert_eq!(r, Err("mode_off".into()));
    }

    #[test]
    fn a_site_rule_covers_its_subdomains_but_not_lookalikes() {
        let p = policy("producer");
        assert!(p.site_allowed("https://m.youtube.com/x"));
        assert!(!p.site_allowed("https://youtube.com.evil.example/"));
        assert!(!p.site_allowed("https://notyoutube.com/"));
        assert!(!p.site_allowed("file:///etc/passwd"));
    }

}
