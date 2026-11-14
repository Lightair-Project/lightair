//! Design tokens and small building blocks (docs/06-design-system.md). Feature code uses these
//! instead of raw colours or sizes. They move into the `lightair-ui` crate as it grows.

use gpui_kit::component::{Icon, Theme, ThemeMode};
use gpui_kit::*;

/// Dark theme tokens. Some are not used by any panel yet.
#[allow(dead_code)]
pub mod tok {
    use gpui_kit::{Hsla, rgb, rgba};

    // Surfaces, darkest to lightest: the window behind the panels, the canvas well, a panel's
    // body, its header band, raised controls, selection.
    pub fn bg_app() -> Hsla {
        rgb(0x0B0B0C).into()
    }
    pub fn bg_canvas_well() -> Hsla {
        rgb(0x101012).into()
    }
    pub fn bg_panel() -> Hsla {
        rgb(0x18181B).into()
    }
    /// Panel headers and tab bars: a band above the panel's body.
    pub fn bg_header() -> Hsla {
        rgb(0x222226).into()
    }
    pub fn bg_raised() -> Hsla {
        rgb(0x26262B).into()
    }
    pub fn bg_selected() -> Hsla {
        rgb(0x33333A).into()
    }
    pub fn border() -> Hsla {
        rgb(0x2B2B30).into()
    }
    /// Between a header band and its panel's body.
    pub fn border_header() -> Hsla {
        rgb(0x34343B).into()
    }
    pub fn border_strong() -> Hsla {
        rgb(0x41414A).into()
    }
    pub fn text() -> Hsla {
        rgb(0xF0F0F2).into()
    }
    pub fn text_secondary() -> Hsla {
        rgb(0xA9A9B1).into()
    }
    pub fn text_tertiary() -> Hsla {
        rgb(0x76767F).into()
    }
    pub fn text_inverse() -> Hsla {
        rgb(0x0E0E0F).into()
    }
    pub fn accent_primary() -> Hsla {
        rgb(0xFFFFFF).into()
    }
    pub fn interactive() -> Hsla {
        rgb(0xEDEDED).into()
    }
    pub fn live() -> Hsla {
        rgb(0xFF3B4A).into()
    }
    pub fn ok() -> Hsla {
        rgb(0x34D399).into()
    }
    pub fn warn() -> Hsla {
        rgb(0xFBBF24).into()
    }
    pub fn error() -> Hsla {
        rgb(0xF87171).into()
    }
    pub fn canvas_selection() -> Hsla {
        rgb(0xFFFFFF).into()
    }
    pub fn canvas_selection_edge() -> Hsla {
        rgb(0x141414).into()
    }
    pub fn canvas_guide() -> Hsla {
        rgb(0xFF4FD8).into()
    }
    /// Safe-area overlays (`canvas.safe-area`).
    pub fn canvas_safe_area() -> Hsla {
        rgba(0xFFFFFF2E).into()
    }
    pub fn canvas_safe_area_fill() -> Hsla {
        rgba(0xFFFFFF14).into()
    }
    /// Rotate gizmo rings (the usual 3D-tool axis colours) and the ring in use.
    pub fn gizmo_x() -> Hsla {
        rgb(0xF0524F).into()
    }
    pub fn gizmo_y() -> Hsla {
        rgb(0x5FD068).into()
    }
    pub fn gizmo_z() -> Hsla {
        rgb(0x4F8BF0).into()
    }
    /// Prefab instances and assets.
    pub fn prefab() -> Hsla {
        rgb(0x5AA9FF).into()
    }
    pub fn gizmo_active() -> Hsla {
        rgb(0xF5D547).into()
    }
    pub fn scrim() -> Hsla {
        rgba(0x00000066).into()
    }
}

/// Applies Lightair's tokens to gpui-component's theme so standard controls match.
pub fn apply_theme(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    let theme = Theme::global_mut(cx);
    let c = &mut theme.colors;
    // Panels: a body, a header band above it, tabs in the band.
    c.background = tok::bg_panel();
    c.tab_bar = tok::bg_header();
    c.tab = tok::bg_header();
    c.tab_active = tok::bg_panel();
    c.tab_foreground = tok::text_tertiary();
    c.tab_active_foreground = tok::text();
    c.title_bar = tok::bg_app();
    c.title_bar_border = tok::border();
    c.sidebar = tok::bg_panel();
    c.sidebar_border = tok::border();
    c.popover = tok::bg_raised();
    c.popover_foreground = tok::text();
    c.list = tok::bg_panel();
    c.list_hover = tok::bg_raised();
    c.list_active = tok::bg_selected();
    c.secondary = tok::bg_raised();
    c.secondary_hover = tok::bg_selected();
    c.secondary_active = tok::border_strong();
    c.secondary_foreground = tok::text();
    c.muted = tok::bg_raised();
    c.input = tok::border_strong();
    c.accent = tok::bg_selected();
    c.accent_foreground = tok::text();
    c.foreground = tok::text();
    c.border = tok::border();
    c.muted_foreground = tok::text_secondary();
    c.button_primary = tok::accent_primary();
    c.button_primary_hover = rgb(0xE6E6E6).into();
    c.button_primary_active = rgb(0xD0D0D0).into();
    c.button_primary_foreground = tok::text_inverse();
    c.button_danger = tok::live();
    c.button_danger_foreground = rgb(0xFFFFFF).into();
    theme.radius = px(6.);
    theme.radius_lg = px(10.);
    // The dock and other components draw from tokens built from these colours.
    theme.tokens = gpui_kit::component::ThemeTokens::from(&theme.colors);
}

/// A dock panel's header: its icon and name.
pub fn panel_title(icon_name: &str, label: &'static str) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_1p5()
        // Colours come from the tab (active or not).
        .child(icon(icon_name).size(px(13.)))
        .child(div().text_size(px(12.)).font_weight(FontWeight::SEMIBOLD).child(label))
}

/// The band behind a dock panel's header.
pub fn panel_header_style() -> gpui_kit::component::dock::TitleStyle {
    gpui_kit::component::dock::TitleStyle { background: tok::bg_header(), foreground: tok::text() }
}

/// Lucide icons bundled by gpui-kit, referenced by file name.
pub fn icon(name: &str) -> Icon {
    Icon::empty().path(SharedString::from(format!("icons/{name}.svg")))
}

/// Icon for an input type (docs/06-design-system.md, Iconography).
pub fn input_icon(kind: &str) -> &'static str {
    match kind {
        lightair_core::kinds::SCREEN => "monitor",
        lightair_core::kinds::CAMERA => "camera",
        "image_source" | "slideshow" => "image",
        "text_ft2_source_v2" => "type",
        "color_source_v3" => "square",
        "ffmpeg_source" | "vlc_source" => "clapperboard",
        "lightair_timer" => "timer",
        lightair_core::kinds::MIC | lightair_core::kinds::DESKTOP_AUDIO | lightair_core::kinds::APP_AUDIO => "mic",
        _ => "layers",
    }
}

/// Small square icon button (24×24 hit target).
pub fn icon_button(id: impl Into<ElementId>, name: &str, color: Hsla) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .justify_center()
        .size(px(24.))
        .rounded(px(4.))
        .cursor_pointer()
        .hover(|s| s.bg(tok::bg_selected()))
        .child(icon(name).size(px(15.)).text_color(color))
}
