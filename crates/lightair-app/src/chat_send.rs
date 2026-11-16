//! Sending chat (docs/plans/live-integrations.md, 2.10) as the person signed in on each
//! platform (Outputs › a destination › Sending). It takes a moment (a connection, a page), so
//! it never runs on the main thread: automations call it on theirs, remote clients on the
//! server's.

use serde_json::{Value, json};

/// `{"text", "platform"?, "channel"?, "reply_to"?}`: sends to the channels being read on
/// `platform` (every one when it's empty), as the remote method `chat.send` answers.
pub fn send(hub: &lightair_live::Hub, p: &Value) -> Result<Value, String> {
    let text = p["text"].as_str().unwrap_or("").trim().to_string();
    if text.is_empty() {
        return Err("nothing to send".into());
    }
    let platform = p["platform"].as_str().unwrap_or("");
    let channel = p["channel"].as_str().unwrap_or("");
    let reply_to = p["reply_to"].as_str().filter(|r| !r.is_empty() && !r.starts_with("test-"));
    let channels: Vec<_> = hub
        .status()
        .into_keys()
        .filter(|c| (platform.is_empty() || c.platform == platform) && (channel.is_empty() || c.channel == channel))
        .collect();
    if channels.is_empty() {
        return Err(if platform.is_empty() { "no chat is connected (Outputs › a destination › Chat)".into() } else { format!("no {platform} chat is connected") });
    }
    let mut sent = 0;
    let mut errors = Vec::new();
    for c in &channels {
        match lightair_live::send::send(&c.platform, &c.channel, &text, reply_to) {
            Ok(()) => sent += 1,
            Err(e) => errors.push(format!("{}: {e}", c.platform)),
        }
    }
    if sent == 0 {
        return Err(errors.join("; "));
    }
    Ok(json!({"sent": sent, "failed": errors}))
}
