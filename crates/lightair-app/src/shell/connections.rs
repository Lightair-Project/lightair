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
                .spawn(async move { SERVICES.iter().map(|(s, _, _)| *s).chain(["kofi", "pulsoid"]).filter_map(|s| crate::secrets::connection_token(s).map(|t| (s, t))).collect::<Vec<_>>() })
                .await;
            let _ = this.update(cx, |this, cx| {
                for (service, token) in found {
                    if service == "pulsoid" {
                        this.start_heart_rate(token, cx);
                        continue;
                    }
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

impl StudioWindow {
    /// Heart rate from Pulsoid: to widgets ("heart_rate") and automations' `{{ vars.heart_rate }}`.
    pub(super) fn start_heart_rate(&mut self, token: String, cx: &mut Context<Self>) {
        if let Some(old) = self.heart_rate.take() {
            old.store(true, std::sync::atomic::Ordering::Release);
        }
        if token.is_empty() {
            cx.notify();
            return;
        }
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (widgets, automations) = (self.studio.read(cx).widgets.clone(), self.automations.clone());
        let last = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        lightair_live::integrations::pulsoid(
            token,
            move |bpm| {
                if let Some(w) = &widgets {
                    w.set_data("heart_rate", serde_json::json!(bpm));
                }
                // the variable (and its file) only when it changes by more than a little
                if last.swap(bpm, std::sync::atomic::Ordering::Relaxed).abs_diff(bpm) >= 2 {
                    automations.set_var("heart_rate", serde_json::json!(bpm));
                }
            },
            stop.clone(),
        );
        self.heart_rate = Some(stop);
        cx.notify();
    }

    pub(super) fn paste_pulsoid(&mut self, cx: &mut Context<Self>) {
        let Some(token) = cx.read_from_clipboard().and_then(|c| c.text()).map(|t| t.trim().to_string()).filter(|t| t.len() > 10 && !t.contains(char::is_whitespace)) else {
            self.studio.update(cx, |s, cx| {
                s.issue = Some("Copy the token first (the clipboard doesn't hold one).".into());
                cx.notify();
            });
            return;
        };
        self.start_heart_rate(token.clone(), cx);
        cx.background_executor().spawn(async move { crate::secrets::set_connection_token("pulsoid", &token) }).detach();
    }

    pub(super) fn forget_pulsoid(&mut self, cx: &mut Context<Self>) {
        self.start_heart_rate(String::new(), cx);
        cx.background_executor().spawn(async move { crate::secrets::set_connection_token("pulsoid", "") }).detach();
    }
}
