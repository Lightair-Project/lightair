//! Backdrops (`lightair_backdrop`, docs/plans/screen-studio.md, "Look and framing"): a gradient
//! behind a screen recording, with a soft rounded shadow where the screen sits, at the canvas's
//! size. Drawn here once per change of its settings (it doesn't move), as libobs's source draws
//! it.
//!
//! Settings: `width`, `height`; `preset` (an index into [`PRESETS`], or -1 for `color_a`..`color_c`
//! as 0xAABBGGRR); `angle` (degrees); `shadow` (bool) with `shadow_x`, `shadow_y`, `shadow_w`,
//! `shadow_h` (the screen's box, canvas pixels), `shadow_radius`, `shadow_blur` and
//! `shadow_opacity`.

use serde_json::Value;

pub(crate) const KIND: &str = "lightair_backdrop";

/// (name, three colours as 0xRRGGBB, angle in degrees).
const PRESETS: &[(&str, [u32; 3], f32)] = &[
    ("Dusk", [0x1E1B4B, 0x6D28D9, 0xF472B6], 35.0),
    ("Ocean", [0x0B3B5C, 0x1D7FA8, 0x7DD3C0], 120.0),
    ("Ember", [0x3B0A0A, 0xB4361C, 0xF6A04D], 60.0),
    ("Mint", [0x0F3D3A, 0x2F9E7A, 0xC8F2D8], 150.0),
    ("Graphite", [0x111214, 0x2A2D33, 0x4A4F59], 90.0),
    ("Sand", [0x8A6A4A, 0xD8BC94, 0xF3E6D0], 20.0),
];

/// What a backdrop draws.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Params {
    pub(crate) width: u32,
    pub(crate) height: u32,
    colors: [[f32; 3]; 3],
    angle: f32,
    /// The screen's box (x, y, w, h), radius, blur, opacity.
    shadow: Option<([f32; 4], f32, f32, f32)>,
}

fn rgb(c: u32) -> [f32; 3] {
    // sRGB colours, as the canvas draws them
    [((c >> 16) & 0xFF) as f32 / 255.0, ((c >> 8) & 0xFF) as f32 / 255.0, (c & 0xFF) as f32 / 255.0]
}

/// 0xAABBGGRR as 0xRRGGBB.
fn from_abgr(c: u32) -> u32 {
    ((c & 0xFF) << 16) | (c & 0xFF00) | ((c >> 16) & 0xFF)
}

impl Params {
    /// From a backdrop's settings, with the source's defaults.
    pub(crate) fn of(s: &Value) -> Params {
        let int = |k: &str, d: i64| s.get(k).and_then(Value::as_i64).unwrap_or(d);
        let float = |k: &str, d: f64| s.get(k).and_then(Value::as_f64).unwrap_or(d) as f32;
        let preset = int("preset", 0);
        let (colors, angle) = match PRESETS.get(preset.max(0) as usize).filter(|_| preset >= 0) {
            Some((_, c, a)) => ([rgb(c[0]), rgb(c[1]), rgb(c[2])], *a),
            None => (
                [
                    rgb(from_abgr(int("color_a", 0) as u32)),
                    rgb(from_abgr(int("color_b", 0) as u32)),
                    rgb(from_abgr(int("color_c", 0) as u32)),
                ],
                float("angle", 0.0),
            ),
        };
        let shadow = s.get("shadow").and_then(Value::as_bool).unwrap_or(false).then(|| {
            (
                [float("shadow_x", 0.0), float("shadow_y", 0.0), float("shadow_w", 0.0), float("shadow_h", 0.0)],
                float("shadow_radius", 24.0),
                float("shadow_blur", 40.0).max(1.0),
                float("shadow_opacity", 0.45).clamp(0.0, 1.0),
            )
        });
        Params {
            width: int("width", 1920).clamp(2, 8192) as u32,
            height: int("height", 1080).clamp(2, 8192) as u32,
            colors,
            angle,
            shadow,
        }
    }

    /// The picture, RGBA (opaque).
    pub(crate) fn draw(&self) -> Vec<u8> {
        let (w, h) = (self.width as usize, self.height as usize);
        let a = self.angle.to_radians();
        let dir = (a.cos(), a.sin());
        let (rect, radius, blur, opacity) = self.shadow.unwrap_or(([0.0; 4], 0.0, 1.0, 0.0));
        let center = (rect[0] + rect[2] * 0.5, rect[1] + rect[3] * 0.5 + blur * 0.35);
        let lerp = |a: [f32; 3], b: [f32; 3], t: f32| {
            [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
        };
        let smoothstep = |e0: f32, e1: f32, x: f32| {
            let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
            t * t * (3.0 - 2.0 * t)
        };
        let mut out = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                // the pixel's centre, as the shader's interpolated uv
                let (u, v) = ((x as f32 + 0.5) / w as f32, (y as f32 + 0.5) / h as f32);
                let t = ((u - 0.5) * dir.0 + (v - 0.5) * dir.1 + 0.5).clamp(0.0, 1.0);
                let lo = lerp(self.colors[0], self.colors[1], (t * 2.0).clamp(0.0, 1.0));
                let hi = lerp(self.colors[1], self.colors[2], (t * 2.0 - 1.0).clamp(0.0, 1.0));
                let mut c = if t >= 0.5 { hi } else { lo };
                if opacity > 0.0 {
                    let p = (u * w as f32, v * h as f32);
                    let q = (
                        (p.0 - center.0).abs() - rect[2] * 0.5 + radius,
                        (p.1 - center.1).abs() - rect[3] * 0.5 + radius,
                    );
                    let d = (q.0.max(0.0).powi(2) + q.1.max(0.0).powi(2)).sqrt() + q.0.max(q.1).min(0.0) - radius;
                    let s = 1.0 - smoothstep(-blur, blur, d);
                    let k = 1.0 - opacity * s;
                    c = [c[0] * k, c[1] * k, c[2] * k];
                }
                let i = (y * w + x) * 4;
                out[i] = (c[0].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                out[i + 1] = (c[1].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                out[i + 2] = (c[2].clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
                out[i + 3] = 255;
            }
        }
        out
    }
}
