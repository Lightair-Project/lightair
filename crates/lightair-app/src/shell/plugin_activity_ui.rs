//! The status bar's plugin activity (plugins.md §4.13): a chip that lights while any plugin uses
//! a permission, one icon per kind, showing the newest notice for a few seconds; clicked, an
//! overlay with every plugin's uses this run (by plugin: each kind, how often, when last, what
//! was refused) and the notices. Never drawn into the program: it's Lightair's interface.

use std::collections::BTreeSet;
use std::time::SystemTime;

use super::*;
use crate::plugin_activity::{self as pa, Kind};

fn kind_icon(k: Kind) -> &'static str {
    match k {
        Kind::Network => "globe",
        Kind::HomeNetwork => "house-wifi",
        Kind::Screen => "monitor",
        Kind::Camera => "camera",
        Kind::Microphone => "mic",
        Kind::Files => "folder-open",
        Kind::Devices => "usb",
        Kind::OtherApp => "app-window",
        Kind::Chat => "message-square",
        Kind::FullAccess => "shield-alert",
    }
}

/// "just now", "2 min ago", "1 h ago".
fn ago(t: SystemTime) -> String {
    let s = t.elapsed().map(|d| d.as_secs()).unwrap_or(0);
    match s {
        0..=9 => "just now".into(),
        10..=59 => format!("{s} s ago"),
        60..=3599 => format!("{} min ago", s / 60),
        _ => format!("{} h ago", s / 3600),
    }
}

impl StudioWindow {
    /// The chip, when a plugin used anything this run (nothing otherwise: the bar stays calm).
    pub(super) fn plugin_activity_chip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let tallies = pa::tallies();
        if tallies.is_empty() && !self.plugin_activity_open {
            return None;
        }
        let lit: BTreeSet<Kind> = pa::lit().into_iter().map(|(_, k)| k).collect();
        let notice = pa::current_notice();
        let refused = notice.as_ref().is_some_and(|n| n.refused);
        let open = self.plugin_activity_open;
        let colour = if refused {
            tok::warn()
        } else if lit.contains(&Kind::FullAccess) || lit.iter().any(|k| k.sensitive()) {
            tok::interactive()
        } else if lit.is_empty() {
            tok::text_tertiary()
        } else {
            tok::text_secondary()
        };
        let mut chip = div()
            .id("plugin-activity-chip")
            .flex()
            .items_center()
            .gap_1()
            .h(px(20.))
            .px_1p5()
            .rounded(px(4.))
            .cursor_pointer()
            .text_color(colour)
            .when(open, |d| d.bg(tok::bg_raised()))
            .hover(|d| d.bg(tok::bg_raised()))
            .on_click(cx.listener(|this, _, _, cx| {
                this.plugin_activity_open = !this.plugin_activity_open;
                cx.notify();
            }));
        if lit.is_empty() {
            chip = chip.child(ui::icon("puzzle").size(px(13.)));
        } else {
            for k in &lit {
                chip = chip.child(ui::icon(kind_icon(*k)).size(px(12.)));
            }
        }
        chip = chip.child(match &notice {
            Some(n) => n.text.clone(),
            None => "Plugins".to_string(),
        });
        Some(div().relative().child(chip).children(open.then(|| self.plugin_activity_overlay(cx))).into_any_element())
    }

    fn plugin_activity_overlay(&self, cx: &mut Context<Self>) -> AnyElement {
        let tallies = pa::tallies();
        let mut list = div().flex().flex_col().gap_3();
        if tallies.is_empty() {
            list = list.child(div().py_3().text_color(tok::text_tertiary()).child("No plugin has used anything yet."));
        }
        let lit: BTreeSet<(String, Kind)> = pa::lit().into_iter().collect();
        for (plugin, kinds) in &tallies {
            let mut rows = div().flex().flex_col().gap_1();
            for (kind, t) in kinds {
                let now = lit.contains(&(plugin.clone(), *kind));
                rows = rows.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(ui::icon(kind_icon(*kind)).size(px(13.)).text_color(if now {
                            tok::interactive()
                        } else {
                            tok::text_secondary()
                        }))
                        .child(div().w(px(110.)).text_color(tok::text()).child(kind.label()))
                        .child(div().flex_1().text_color(tok::text_tertiary()).child(match t.last {
                            Some(_) if now => "in use now".to_string(),
                            Some(last) => format!(
                                "{} · {}",
                                if t.count == 1 { "once".to_string() } else { format!("{} times", t.count) },
                                ago(last)
                            ),
                            None => String::new(),
                        }))
                        .children(
                            (t.refused > 0)
                                .then(|| div().text_color(tok::warn()).child(format!("{} refused", t.refused))),
                        ),
                );
            }
            // the latest few uses, so "what did it reach" has an answer
            let log = pa::log(plugin);
            let recent: Vec<_> = log.iter().rev().take(6).collect();
            let mut detail =
                div().flex().flex_col().gap_0p5().pl(px(21.)).text_size(px(10.5)).text_color(tok::text_tertiary());
            for u in recent {
                let line = if u.allowed { u.what.clone() } else { format!("{} — refused: {}", u.what, u.reason) };
                detail = detail.child(div().when(!u.allowed, |d| d.text_color(tok::warn())).child(line));
            }
            list = list.child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1p5()
                    .child(div().text_color(tok::text()).font_weight(FontWeight::SEMIBOLD).child(plugin.clone()))
                    .child(rows)
                    .child(detail),
            );
        }
        let full_access: Vec<String> =
            tallies.iter().filter(|(_, k)| k.contains_key(&Kind::FullAccess)).map(|(p, _)| p.clone()).collect();
        div()
            .id("plugin-activity-overlay")
            .absolute()
            .bottom(px(26.))
            .left_0()
            .w(px(380.))
            .max_h(px(480.))
            .overflow_y_scroll()
            .p_3()
            .rounded(px(8.))
            .border_1()
            .border_color(tok::border())
            .bg(tok::bg_panel())
            .shadow_lg()
            .text_size(px(11.5))
            .on_mouse_down_out(cx.listener(|this, _, _, cx| {
                this.plugin_activity_open = false;
                cx.notify();
            }))
            .child(div().pb_2().text_color(tok::text()).font_weight(FontWeight::SEMIBOLD).child("Plugin activity"))
            .children((!full_access.is_empty()).then(|| {
                div().mb_2().p_2().rounded(px(6.)).bg(tok::bg_raised()).text_color(tok::text_secondary()).child(
                    format!(
                        "{} running with full access: Lightair can't see what it does beyond its calls into Lightair.",
                        full_access.join(", ")
                    ),
                )
            }))
            .child(list)
            .into_any_element()
    }
}
