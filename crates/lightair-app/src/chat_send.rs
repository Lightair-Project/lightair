//! Sending chat (docs/plans/live-integrations.md, 2.10): typed into each platform's own chat
//! page, signed in (a hidden page, so no app approval is needed). Until a platform's sending is
//! set up, sending there says so.

/// Sends `text` to `platform`'s chat ("" : every channel being read): how many it went to.
pub fn send(hub: &lightair_live::Hub, platform: &str, text: &str) -> Result<usize, String> {
    let _ = text;
    let channels: Vec<_> = hub.status().into_keys().filter(|c| platform.is_empty() || c.platform == platform).collect();
    if channels.is_empty() {
        return Err(if platform.is_empty() { "no chat is connected".into() } else { format!("no {platform} chat is connected") });
    }
    Err(format!("sending to {} chat isn't set up yet", if platform.is_empty() { "this" } else { platform }))
}
