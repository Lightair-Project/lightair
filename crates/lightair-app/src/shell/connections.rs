//! Settings › Connections: tip services connected with the streamer's own token from each
//! service's dashboard (no app approval). Tokens go to the Keychain, read back off the main
//! thread at start (a Keychain prompt mustn't freeze the window).

use gpui_kit::*;

use super::StudioWindow;

/// (service, name, where its token is).
pub(super) const SERVICES: &[(&str, &str, &str)] = &[
    ("streamlabs", "Streamlabs", "Its Socket API token: streamlabs.com › Settings › API Settings › API Tokens"),
    ("streamelements", "StreamElements", "Its JWT token: streamelements.com › Account › Channels › Show secrets"),
];

impl StudioWindow {
    /// Connects the services that have a token saved (at start).
    pub(super) fn load_connections(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            let found = cx
                .background_executor()
                .spawn(async move { SERVICES.iter().map(|(s, _, _)| *s).chain(["kofi"]).filter_map(|s| crate::secrets::connection_token(s).map(|t| (s, t))).collect::<Vec<_>>() })
                .await;
            let _ = this.update(cx, |this, cx| {
                for (service, token) in found {
                    lightair_live::donations::set_token(service, &token);
                    this.studio.update(cx, |s, _| s.set_service(service, true));
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Saves the token on the clipboard for `service` and connects it.
    pub(super) fn paste_connection(&mut self, service: &'static str, cx: &mut Context<Self>) {
        let Some(token) = cx.read_from_clipboard().and_then(|c| c.text()).map(|t| t.trim().to_string()).filter(|t| t.len() > 10 && !t.contains(char::is_whitespace)) else {
            self.studio.update(cx, |s, cx| {
                s.issue = Some("Copy the token first (the clipboard doesn't hold one).".into());
                cx.notify();
            });
            return;
        };
        lightair_live::donations::set_token(service, &token);
        self.studio.update(cx, |s, _| {
            // a new token: the reader starts again with it
            s.set_service(service, false);
            s.set_service(service, true);
        });
        cx.background_executor()
            .spawn(async move {
                if let Err(e) = crate::secrets::set_connection_token(service, &token) {
                    tracing::warn!(%e, "couldn't keep the token");
                }
            })
            .detach();
        cx.notify();
    }

    pub(super) fn disconnect(&mut self, service: &'static str, cx: &mut Context<Self>) {
        lightair_live::donations::set_token(service, "");
        self.studio.update(cx, |s, _| s.set_service(service, false));
        cx.background_executor().spawn(async move { crate::secrets::set_connection_token(service, "") }).detach();
        cx.notify();
    }
}

/// Ko-fi through the relay: what's kept (Keychain, "kofi"): {"relay", "key", "verify"}.
pub(super) fn kofi_settings() -> serde_json::Value {
    lightair_live::donations::kofi_settings()
}

impl StudioWindow {
    fn save_kofi(&mut self, v: serde_json::Value, cx: &mut Context<Self>) {
        let text = v.to_string();
        lightair_live::donations::set_token("kofi", &text);
        let ready = v["relay"].as_str().is_some_and(|r| !r.is_empty()) && v["verify"].as_str().is_some_and(|r| !r.is_empty());
        self.studio.update(cx, |s, _| {
            s.set_service("kofi", false);
            s.set_service("kofi", ready);
        });
        cx.background_executor().spawn(async move { crate::secrets::set_connection_token("kofi", &text) }).detach();
        cx.notify();
    }

    /// From the clipboard: the relay's address (an https address) or Ko-fi's verification token.
    pub(super) fn paste_kofi(&mut self, what: &str, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()).map(|t| t.trim().to_string()).filter(|t| !t.is_empty() && !t.contains(char::is_whitespace)) else {
            self.studio.update(cx, |s, cx| {
                s.issue = Some("Copy it first (the clipboard is empty).".into());
                cx.notify();
            });
            return;
        };
        let mut v = kofi_settings();
        if v["key"].as_str().is_none_or(str::is_empty) {
            let mut b = [0u8; 16];
            let _ = getrandom::fill(&mut b);
            v["key"] = serde_json::json!(b.iter().map(|x| format!("{x:02x}")).collect::<String>());
        }
        if what == "relay" {
            if !text.starts_with("https://") {
                self.studio.update(cx, |s, cx| {
                    s.issue = Some("The relay's address starts with https:// (what `npx wrangler deploy` printed).".into());
                    cx.notify();
                });
                return;
            }
            v["relay"] = serde_json::json!(text.trim_end_matches('/'));
        } else {
            v["verify"] = serde_json::json!(text);
        }
        self.save_kofi(v, cx);
    }
}
