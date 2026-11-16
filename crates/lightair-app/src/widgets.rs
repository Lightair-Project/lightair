//! Widgets in the app (docs/plans/live-integrations.md, 2.5): the widget server started with
//! the workspace, widget pages' addresses resolved for Chromium, and the platform icons widgets
//! show (the same badges as the Outputs and Chat panels).

use gpui_kit::AssetSource;
use lightair_widgets::Widgets;

use crate::platforms;

pub fn start(hub: &lightair_live::Hub) -> Option<Widgets> {
    let dir = lightair_platform::paths::data_dir().join("widgets");
    match Widgets::start(hub.clone(), dir) {
        Ok(w) => {
            let port = w.port();
            lightair_web::set_url_resolver(move |url| lightair_widgets::resolve(url, port));
            w.set_icons(Box::new(icon_svg));
            crate::now_playing::start(w.clone());
            Some(w)
        }
        Err(e) => {
            tracing::warn!(%e, "widgets can't be served");
            None
        }
    }
}

/// A platform's badge as an SVG: its colour, its logo (or mark) on it.
fn icon_svg(platform: &str) -> Option<Vec<u8>> {
    let p = platforms::by_id(platform)?;
    let ink = if matches!(p.id, "kick" | "chzzk") { "#0B0B0C" } else { "#FFFFFF" };
    let inner = p
        .logo
        .and_then(|logo| crate::assets::Assets.load(&format!("icons/{logo}.svg")).ok().flatten())
        .and_then(|bytes| {
            let svg = String::from_utf8_lossy(&bytes).into_owned();
            let start = svg.find('>')? + 1;
            let end = svg.rfind("</svg>")?;
            Some(format!(r#"<g transform="translate(5 5) scale(0.5833)" fill="{ink}">{}</g>"#, &svg[start..end]))
        })
        .unwrap_or_else(|| {
            format!(
                r#"<text x="12" y="16" text-anchor="middle" font-family="-apple-system, Segoe UI, sans-serif" font-weight="700" font-size="{}" fill="{ink}">{}</text>"#,
                if p.mark.chars().count() > 2 { 7 } else { 10 },
                p.mark
            )
        });
    Some(format!(r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24"><rect width="24" height="24" rx="5.5" fill="#{:06X}"/>{inner}</svg>"##, p.color).into_bytes())
}
