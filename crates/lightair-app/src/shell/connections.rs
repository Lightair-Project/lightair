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
                .spawn(async move { SERVICES.iter().filter_map(|(s, _, _)| crate::secrets::connection_token(s).map(|t| (*s, t))).collect::<Vec<_>>() })
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
