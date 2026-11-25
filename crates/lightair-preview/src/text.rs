//! Text and timer objects (libobs's `text_ft2_source_v2` settings) drawn by the compositor as
//! libobs draws them, with lightair-render's text.
//!
//! - **Faces**: libobs's text source finds a font by its name among the files in the system's
//!   font folders (macOS: `~/Library/Fonts`, `/Library/Fonts`, `/Network/Library/Fonts`,
//!   `/System/Library/Fonts`; Windows: the Windows folder's `Fonts` and the user's own fonts;
//!   and their subfolders). [`install_fonts`] gives lightair-render the same files, with the
//!   desktop's own text face as the sans-serif for text that names none.
//! - **Size**: the font size is the em in pixels in both.
//! - **Layout**: lines start at the left (libobs's text has no alignment); they wrap at the
//!   custom width when word wrap is on, and the picture is at least that wide when a custom
//!   width is set (100 or more, as libobs takes it).
//! - **Colour**: `color1` at the top of each line to `color2` at its bottom, with their alpha.
//! - **Outline**: the letters drawn in black 2 pixels around, in eight directions; **drop
//!   shadow**: the letters in black 4 pixels right and down; both under the letters, and the
//!   picture grows right and down by 4 (one of them) or 6 (both), as libobs's does.

use std::path::PathBuf;

use lightair_engine::pictures::text_style;
use lightair_fonts::FontSet;
use lightair_render::generate;
use serde_json::Value;

/// The face text that names none is drawn in (the desktop's own default text face).
#[cfg(target_os = "macos")]
const DEFAULT_FACE: &str = "Helvetica Neue";
/// The face text that names none is drawn in (the desktop's own default text face).
#[cfg(windows)]
const DEFAULT_FACE: &str = "Segoe UI";

/// The system's font folders, as libobs's text source looks in them.
#[cfg(target_os = "macos")]
fn font_folders() -> Vec<Option<PathBuf>> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    vec![
        home.map(|h| h.join("Library/Fonts")),
        Some(PathBuf::from("/Library/Fonts")),
        Some(PathBuf::from("/Network/Library/Fonts")),
        Some(PathBuf::from("/System/Library/Fonts")),
    ]
}

/// The system's font folders, as libobs's text source looks in them.
#[cfg(windows)]
fn font_folders() -> Vec<Option<PathBuf>> {
    let windows = std::env::var_os("WINDIR").or_else(|| std::env::var_os("SystemRoot")).map(PathBuf::from);
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    vec![windows.map(|w| w.join("Fonts")), local.map(|l| l.join("Microsoft/Windows/Fonts"))]
}

/// Draws text from now on with the fonts libobs's text source finds on this computer. Called
/// once, before the first compositor draws any text (lightair-render keeps the system's own
/// fonts otherwise).
pub(crate) fn install_fonts() {
    let mut set = FontSet::new();
    let folders = font_folders();
    for folder in folders.into_iter().flatten().filter(|f| f.is_dir()) {
        set.add_folder(&folder);
    }
    if set.is_empty() {
        tracing::warn!("compositor: no fonts in the system's font folders; text keeps the system's own");
        return;
    }
    let sans = [DEFAULT_FACE, "Helvetica", "Arial"].into_iter().find(|f| set.has_family(f));
    if let Some(sans) = sans {
        set.set_sans(sans);
    }
    tracing::info!(faces = set.len(), "compositor: text drawn with the system's font folders");
    generate::set_fonts(set);
}

/// What decides a text object's picture: its settings and the text it shows.
pub(crate) fn key(settings: &Value, text: &str) -> String {
    format!("{text}\u{0}{settings}")
}

/// A text object's picture (`settings` as libobs's text source has them, showing `text`):
/// premultiplied RGBA and its size.
pub(crate) fn draw(settings: &Value, text: &str) -> (Vec<u8>, (u32, u32)) {
    let mut style = text_style(settings, text.to_string());
    // libobs's text lines start at the left; an alignment the document gives is kept
    if settings.get("align").is_none() {
        style.align = 0;
    }
    if flags(settings) & 2 != 0 {
        style.font = style.font.take().map(|f| f.italic());
    }
    // wrapped only at a custom width with word wrap on; otherwise one line per line of text
    let custom = settings["custom_width"].as_u64().filter(|w| *w >= 100).map(|w| w as f32);
    let wrap = settings["word_wrap"].as_bool().unwrap_or(false);
    style.width = match custom {
        Some(w) if wrap => w,
        _ => 65_536.0,
    };
    // the outline is drawn here (libobs's: 2 pixels, black), the letters white and coloured below
    style.outline = None;
    style.color = 0xFFFFFF;
    let has_box = style.background.is_some();
    let glyphs = generate::text(&style);
    let (gw, gh) = glyphs.size;
    // where generate::text puts the first line (its margin around the words)
    let pad = if has_box { style.size * 0.35 } else { style.size * 0.12 };
    let top = pad.round() as i32;
    let line = style.size.max(4.0) * 1.2;

    let outline = settings["outline"].as_bool().unwrap_or(false);
    let shadow = settings["drop_shadow"].as_bool().unwrap_or(false);
    let extra = match (outline, shadow) {
        (true, true) => 6,
        (true, false) | (false, true) => 4,
        (false, false) => 0,
    };
    let width = match custom {
        Some(w) => gw.max((w + 2.0 * pad).ceil() as u32),
        None => gw,
    };
    let (w, h) = (width + extra, gh + extra);
    // the words' place across a custom width, by their alignment
    let x0 = match (custom, style.align) {
        (Some(_), 1) => (width - gw) as i32 / 2,
        (Some(_), 2) => (width - gw) as i32,
        _ => 0,
    };
    let mut out = Canvas { w, h, px: vec![0; (w * h * 4) as usize] };
    if shadow {
        out.stamp(&glyphs.rgba, (gw, gh), x0 + 4, 4, [0, 0, 0, 255]);
    }
    if outline {
        for (dx, dy) in [(-2, 0), (-2, -2), (0, -2), (2, -2), (2, 0), (2, 2), (0, 2), (-2, 2)] {
            out.stamp(&glyphs.rgba, (gw, gh), x0 + dx, dy, [0, 0, 0, 255]);
        }
    }
    let (top_colour, bottom_colour) = colours(settings);
    let shade = |y: u32| -> [f32; 4] {
        if top_colour == bottom_colour {
            return top_colour;
        }
        // through each line, top to bottom
        let within = ((y as f32 - top as f32) / line).max(0.0).fract();
        let mut c = [0.0; 4];
        for k in 0..4 {
            c[k] = top_colour[k] + (bottom_colour[k] - top_colour[k]) * within;
        }
        c
    };
    out.letters(&glyphs.rgba, (gw, gh), x0, shade);
    (out.px, (w, h))
}

fn flags(settings: &Value) -> i64 {
    settings["font"]["flags"].as_i64().unwrap_or(0)
}

/// `color1` and `color2` (libobs's 0xAABBGGRR) as straight RGBA, 0–1.
fn colours(settings: &Value) -> ([f32; 4], [f32; 4]) {
    let colour = |key: &str| {
        let v = settings[key].as_u64().unwrap_or(0xFFFF_FFFF) as u32;
        [
            (v & 0xFF) as f32 / 255.0,
            ((v >> 8) & 0xFF) as f32 / 255.0,
            ((v >> 16) & 0xFF) as f32 / 255.0,
            (v >> 24) as f32 / 255.0,
        ]
    };
    let top = colour("color1");
    let bottom = if settings.get("color2").is_some() { colour("color2") } else { top };
    (top, bottom)
}

/// A premultiplied RGBA picture being made.
struct Canvas {
    w: u32,
    h: u32,
    px: Vec<u8>,
}

impl Canvas {
    /// `src`'s coverage (its alpha) in one colour, over what's there, its top-left at (x, y).
    fn stamp(&mut self, src: &[u8], (sw, sh): (u32, u32), x: i32, y: i32, rgba: [u8; 4]) {
        for sy in 0..sh as i32 {
            let ty = sy + y;
            if ty < 0 || ty >= self.h as i32 {
                continue;
            }
            for sx in 0..sw as i32 {
                let tx = sx + x;
                if tx < 0 || tx >= self.w as i32 {
                    continue;
                }
                let a = src[((sy as u32 * sw + sx as u32) * 4 + 3) as usize] as u32 * rgba[3] as u32 / 255;
                if a == 0 {
                    continue;
                }
                let d = ((ty as u32 * self.w + tx as u32) * 4) as usize;
                for k in 0..3 {
                    self.px[d + k] = (rgba[k] as u32 * a / 255 + self.px[d + k] as u32 * (255 - a) / 255) as u8;
                }
                self.px[d + 3] = (a + self.px[d + 3] as u32 * (255 - a) / 255) as u8;
            }
        }
    }

    /// The letters (`src`, premultiplied, plain glyphs in white) over what's there, plain
    /// glyphs in the colour `shade` gives each row; colour glyphs (emoji) as they are.
    fn letters(&mut self, src: &[u8], (sw, sh): (u32, u32), x: i32, shade: impl Fn(u32) -> [f32; 4]) {
        for sy in 0..sh {
            if sy >= self.h {
                break;
            }
            let c = shade(sy);
            for sx in 0..sw as i32 {
                let tx = sx + x;
                if tx < 0 || tx >= self.w as i32 {
                    continue;
                }
                let s = ((sy * sw + sx as u32) * 4) as usize;
                let p = [src[s], src[s + 1], src[s + 2], src[s + 3]];
                if p[3] == 0 {
                    continue;
                }
                // a plain glyph is white: its colour channels equal its alpha
                let plain = p[0] == p[3] && p[1] == p[3] && p[2] == p[3];
                let p = if plain {
                    let a = p[3] as f32 / 255.0 * c[3];
                    [
                        (c[0] * a * 255.0).round() as u32,
                        (c[1] * a * 255.0).round() as u32,
                        (c[2] * a * 255.0).round() as u32,
                        (a * 255.0).round() as u32,
                    ]
                } else {
                    [p[0] as u32, p[1] as u32, p[2] as u32, p[3] as u32]
                };
                let d = ((sy * self.w + tx as u32) * 4) as usize;
                let keep = 255 - p[3].min(255);
                for k in 0..4 {
                    self.px[d + k] = (p[k] + self.px[d + k] as u32 * keep / 255).min(255) as u8;
                }
            }
        }
    }
}
