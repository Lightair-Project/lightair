//! Design tokens and small building blocks (docs/06-design-system.md). Feature code uses these
//! instead of raw colours or sizes. They move into the `lightair-ui` crate as it grows.

use gpui_kit::component::{Icon, Theme, ThemeMode};
use gpui_kit::*;

/// Dark theme tokens. Some are not used by any panel yet.
#[allow(dead_code)]
pub mod tok {
    use gpui_kit::{Hsla, rgb, rgba};

    pub fn bg_app() -> Hsla {
        rgb(0x141414).into()
    }
    pub fn bg_panel() -> Hsla {
        rgb(0x1B1B1C).into()
    }
    pub fn bg_raised() -> Hsla {
        rgb(0x232325).into()
    }
    pub fn bg_selected() -> Hsla {
        rgb(0x2B2B2E).into()
    }
    pub fn bg_canvas_well() -> Hsla {
        rgb(0x0E0E0F).into()
    }
    pub fn border() -> Hsla {
        rgb(0x2A2A2D).into()
    }
    pub fn border_strong() -> Hsla {
        rgb(0x3A3A3E).into()
    }
    pub fn text() -> Hsla {
        rgb(0xEDEDED).into()
    }
    pub fn text_secondary() -> Hsla {
        rgb(0xA1A1A6).into()
    }
    pub fn text_tertiary() -> Hsla {
        rgb(0x6E6E73).into()
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
    c.background = tok::bg_app();
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
}

/// Lucide icons bundled by gpui-kit, referenced by file name.
pub fn icon(name: &str) -> Icon {
    Icon::empty().path(SharedString::from(format!("icons/{name}.svg")))
}

/// Icon for an input type (docs/06-design-system.md, Iconography).
pub fn input_icon(kind: &str) -> &'static str {
    match kind {
        "screen_capture" | "display_capture" => "monitor",
        "window_capture" => "app-window",
        "macos-avcapture" | "macos-avcapture-fast" | "av_capture_input_v2" | "av_capture_input" => "camera",
        "image_source" | "slideshow" => "image",
        "text_ft2_source_v2" => "type",
        "color_source_v3" => "square",
        "ffmpeg_source" | "vlc_source" => "clapperboard",
        "lightair_timer" => "timer",
        "coreaudio_input_capture" | "sck_audio_capture" => "mic",
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
