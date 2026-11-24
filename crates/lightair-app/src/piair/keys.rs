//! The AI co-host's API keys (Settings → Connections): one per service, kept in the Keychain
//! where PiAir reads them (service "Lightair PiAir": `voice:<vendor>` for voices,
//! `provider:<id>` for brains, as Pi's credential JSON), checked with a light request when pasted.

use serde_json::{Value, json};

use super::KEYCHAIN_SERVICE;

/// What a key is for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Use {
    Voice,
    Brain,
    /// Google's key: Gemini models and the Gemini voice.
    Both,
}

/// (id, name, use, where to get the key).
pub const SERVICES: &[(&str, &str, Use, &str)] = &[
    (
        "elevenlabs",
        "ElevenLabs",
        Use::Voice,
        "elevenlabs.io › Developers › API keys. Give it Text to Speech, and Voices (read) for the voice picker.",
    ),
    ("cartesia", "Cartesia", Use::Voice, "play.cartesia.ai › API keys."),
    (
        "google",
        "Google Gemini",
        Use::Both,
        "aistudio.google.com › Get API key. Used for Gemini models and the Gemini voice.",
    ),
    (
        "anthropic",
        "Anthropic (Claude)",
        Use::Brain,
        "console.anthropic.com › API keys. With Claude Pro or Max, sign in under Subscriptions instead (one or the other).",
    ),
    (
        "openai",
        "OpenAI",
        Use::Brain,
        "platform.openai.com › API keys. With a ChatGPT subscription, sign in under Subscriptions instead (one or the other).",
    ),
    ("openrouter", "OpenRouter", Use::Brain, "openrouter.ai › Keys."),
    ("groq", "Groq", Use::Brain, "console.groq.com › API keys."),
    ("xai", "xAI (Grok)", Use::Brain, "console.x.ai › API keys."),
];

/// Subscriptions the co-host's brain can use instead of a key, signed in on the provider's own
/// page (Pi's OAuth): (Pi provider id, name, how it goes).
pub const SUBSCRIPTIONS: &[(&str, &str, &str)] = &[
    (
        "anthropic",
        "Claude Pro or Max",
        "Sign in with your Claude account: its page opens, then copy the code it shows and choose Paste code. Replaces an Anthropic key (one or the other).",
    ),
    (
        "openai",
        "ChatGPT",
        "Sign in with your ChatGPT account: its page opens and comes back here by itself. Your co-host then uses OpenAI's models on your subscription. Replaces an OpenAI key (one or the other).",
    ),
    ("github-copilot", "GitHub Copilot", "Sign in with GitHub: its page opens; enter the code shown here."),
    (
        "xai",
        "SuperGrok or X Premium",
        "Sign in with your X account: its page opens and comes back here by itself. Replaces an xAI key (one or the other).",
    ),
];

/// The name of a subscription, for messages.
pub fn subscription_name(provider: &str) -> &'static str {
    SUBSCRIPTIONS.iter().find(|s| s.0 == provider).map_or("the subscription", |s| s.1)
}

/// Whether a subscription is signed in (Pi's OAuth credential in the Keychain). Off the main
/// thread.
pub fn signed_in(provider: &str) -> bool {
    keyring::Entry::new(KEYCHAIN_SERVICE, &format!("provider:{provider}"))
        .ok()
        .and_then(|e| e.get_password().ok())
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .is_some_and(|v| v["type"] == "oauth")
}

/// Signs a subscription out: its OAuth credential goes (a key saved for the provider stays).
pub fn sign_out(provider: &str) -> Result<(), String> {
    if !signed_in(provider) {
        return Ok(());
    }
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, &format!("provider:{provider}")).map_err(|e| e.to_string())?;
    entry.delete_credential().map_err(|e| e.to_string())
}

/// The voice vendor's key service for a co-host voice (`gemini` uses Google's).
pub fn service_for_voice(vendor: &str) -> &'static str {
    match vendor {
        "elevenlabs" => "elevenlabs",
        "cartesia" => "cartesia",
        _ => "google",
    }
}

/// The name of a service, for messages.
pub fn name_of(id: &str) -> &'static str {
    SERVICES.iter().find(|s| s.0 == id).map_or("the AI provider", |s| s.1)
}

fn entry_name(id: &str) -> String {
    match SERVICES.iter().find(|s| s.0 == id).map(|s| s.2) {
        Some(Use::Voice) => format!("voice:{id}"),
        _ => format!("provider:{id}"),
    }
}

/// The key saved for a service, if any. Reads the Keychain: off the main thread.
pub fn get(id: &str) -> Option<String> {
    let raw = keyring::Entry::new(KEYCHAIN_SERVICE, &entry_name(id)).ok()?.get_password().ok()?;
    if entry_name(id).starts_with("provider:") {
        // Pi's credential: {"type": "api_key", "key": …} (an OAuth sign-in isn't a key)
        let v: Value = serde_json::from_str(&raw).ok()?;
        return (v["type"] == "api_key")
            .then(|| v["key"].as_str().map(str::to_owned))
            .flatten()
            .filter(|k| !k.is_empty());
    }
    Some(raw).filter(|k| !k.is_empty())
}

/// Saves (or, with `None`, removes) a service's key.
pub fn set(id: &str, key: Option<&str>) -> Result<(), String> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, &entry_name(id)).map_err(|e| e.to_string())?;
    match key {
        Some(k) if entry_name(id).starts_with("provider:") => {
            entry.set_password(&json!({"type": "api_key", "key": k}).to_string()).map_err(|e| e.to_string())
        }
        Some(k) => entry.set_password(k).map_err(|e| e.to_string()),
        None => {
            let _ = entry.delete_credential();
            Ok(())
        }
    }
}

/// Whether a key works: a light request to the service. `Ok` with a note when it works but
/// can't do everything (an ElevenLabs key without Voices read).
pub fn check(id: &str, key: &str) -> Result<Option<String>, String> {
    let agent = lightair_live::net::agent();
    let get = |url: &str, headers: &[(&str, String)]| -> Result<(u16, String), String> {
        let mut req = agent.get(url);
        for (k, v) in headers {
            req = req.header(*k, v.as_str());
        }
        let mut res = req.call().map_err(|e| format!("couldn't reach it: {e}"))?;
        let status = res.status().as_u16();
        let body = res.body_mut().read_to_string().unwrap_or_default();
        Ok((status, body))
    };
    let bearer = format!("Bearer {key}");
    let (status, body) = match id {
        "elevenlabs" => get("https://api.elevenlabs.io/v2/voices?page_size=1", &[("xi-api-key", key.into())])?,
        "cartesia" => get(
            "https://api.cartesia.ai/voices?limit=1",
            &[("X-API-Key", key.into()), ("Cartesia-Version", "2026-08-14".into())],
        )?,
        "google" => get(
            "https://generativelanguage.googleapis.com/v1beta/models?pageSize=1",
            &[("x-goog-api-key", key.into())],
        )?,
        "anthropic" => get(
            "https://api.anthropic.com/v1/models?limit=1",
            &[("x-api-key", key.into()), ("anthropic-version", "2023-06-01".into())],
        )?,
        "openai" => get("https://api.openai.com/v1/models", &[("Authorization", bearer)])?,
        "openrouter" => get("https://openrouter.ai/api/v1/key", &[("Authorization", bearer)])?,
        "groq" => get("https://api.groq.com/openai/v1/models", &[("Authorization", bearer)])?,
        "xai" => get("https://api.x.ai/v1/models", &[("Authorization", bearer)])?,
        _ => return Ok(None),
    };
    match status {
        200..=299 => Ok(None),
        // ElevenLabs: a restricted key without Voices read still speaks
        401 | 403 if id == "elevenlabs" && body.contains("missing_permissions") => {
            Ok(Some("Works for speech; give it Voices (read) to pick a voice".into()))
        }
        401 | 403 => Err("The service didn't accept this key".into()),
        429 => Ok(Some("Works, but it's over its quota right now".into())),
        s => Err(format!("The service answered {s}")),
    }
}
