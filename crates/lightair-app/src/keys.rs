//! Where a destination's stream key comes from when it's going live: the key pasted for it (in
//! the credential store), or the account signed in on the platform (docs/11, "Destinations"):
//! Twitch's own key, YouTube's reusable stream, or a new YouTube broadcast each time. A fetched
//! key is used for that stream and not stored.

use std::collections::HashMap;
use std::sync::Mutex;

use lightair_model::{AccountKey, Destination, Id};

/// Where to send, once the key is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The server to use instead of the destination's own (YouTube names its own).
    pub server: Option<String>,
    pub key: String,
}

/// The YouTube broadcasts made for destinations that are on air, to end with them.
static BROADCASTS: Mutex<Option<HashMap<Id, String>>> = Mutex::new(None);

/// Whether the platform has an official key to fetch (and so a choice to make).
pub fn offers_account_key(platform: &str) -> bool {
    matches!(platform, "twitch" | "youtube" | "youtube-shorts")
}

/// Whether the destination can start without a key being pasted: it takes its account's.
pub fn takes_account_key(d: &Destination) -> bool {
    d.options.account_key != AccountKey::Pasted
}

/// Whether the destination has what it needs to go live as far as its key goes.
pub fn has_key(d: &Destination) -> bool {
    takes_account_key(d) || crate::secrets::stream_key(d.id).is_some()
}

fn platform_of(d: &Destination) -> &str {
    d.platform.as_deref().unwrap_or("custom")
}

/// The key (and server) for a destination about to go live. Waits on the platform for an
/// account's: call it off the main thread. `title` names a new YouTube broadcast.
pub fn resolve(d: &Destination, title: &str) -> Result<Resolved, String> {
    match d.options.account_key {
        AccountKey::Pasted => crate::secrets::stream_key(d.id)
            .map(|key| Resolved { server: None, key })
            .ok_or_else(|| format!("No stream key saved for {}.", d.name)),
        AccountKey::Account => account(d),
        AccountKey::NewBroadcast => {
            if !platform_of(d).starts_with("youtube") {
                return Err(format!("{} can't make a broadcast: that's YouTube's.", d.name));
            }
            let ingest = lightair_live::official::youtube_create_broadcast(title, "unlisted")
                .map_err(|e| format!("{}: {e}", d.name))?;
            if let Some(id) = &ingest.broadcast {
                BROADCASTS.lock().unwrap().get_or_insert_with(HashMap::new).insert(d.id, id.clone());
            }
            Ok(Resolved { server: Some(ingest.url), key: ingest.key })
        }
    }
}

fn account(d: &Destination) -> Result<Resolved, String> {
    let platform = platform_of(d);
    match platform {
        "twitch" => lightair_live::official::stream_key("twitch")
            .map(|key| Resolved { server: None, key })
            .map_err(|e| format!("{}: {e}", d.name)),
        p if p.starts_with("youtube") => lightair_live::official::youtube_ingest()
            .map(|(_, i)| Resolved { server: Some(i.url), key: i.key })
            .map_err(|e| format!("{}: {e}", d.name)),
        other => Err(format!("{}: {other} has no stream key to fetch through an account.", d.name)),
    }
}

/// A destination stopped: the YouTube broadcast made for it ends too (in the background).
pub fn finish(destination: Id) {
    let id = BROADCASTS.lock().unwrap().as_mut().and_then(|m| m.remove(&destination));
    if let Some(id) = id {
        std::thread::spawn(move || {
            if let Err(e) = lightair_live::official::youtube_end_broadcast(&id) {
                tracing::warn!(%e, "ending the YouTube broadcast");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dest(platform: &str, account_key: AccountKey) -> Destination {
        Destination {
            id: Id::new(),
            name: "Channel".into(),
            server: "rtmp://example.com/live".into(),
            enabled: true,
            canvas: None,
            platform: Some(platform.into()),
            encoding: None,
            audio_mix: None,
            vod_mix: None,
            chat: None,
            options: lightair_model::StreamOptions { account_key, ..Default::default() },
        }
    }

    #[test]
    fn a_destination_on_its_account_needs_no_pasted_key() {
        assert!(takes_account_key(&dest("twitch", AccountKey::Account)));
        assert!(takes_account_key(&dest("youtube", AccountKey::NewBroadcast)));
        assert!(!takes_account_key(&dest("twitch", AccountKey::Pasted)));
        assert!(offers_account_key("youtube-shorts") && offers_account_key("twitch"));
        assert!(!offers_account_key("kick") && !offers_account_key("custom"));
    }

    #[test]
    fn a_broadcast_is_youtubes_and_a_key_can_not_come_from_elsewhere() {
        let e = resolve(&dest("twitch", AccountKey::NewBroadcast), "Show").unwrap_err();
        assert!(e.contains("YouTube"), "{e}");
        let e = resolve(&dest("kick", AccountKey::Account), "Show").unwrap_err();
        assert!(e.contains("no stream key to fetch"), "{e}");
    }
}
