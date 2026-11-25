//! Where things pinned to an avatar go on the canvas (docs/plans/avatars.md, "Accessories and
//! pins"): an anchor's place in the avatar's picture ([`Spot`]) and the avatar's box on the
//! canvas ([`Placement`]), and a point of the picture on the canvas ([`on_canvas`]).

use lightair_model::Fit;

/// An anchor in the avatar's picture: pixels, its turn (radians, clockwise as seen) and its size
/// (only ever compared with itself at rest).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spot {
    pub x: f32,
    pub y: f32,
    pub angle: f32,
    pub scale: f32,
}

/// A box in canvas pixels: its centre, size and rotation (degrees clockwise).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub cx: f32,
    pub cy: f32,
    pub w: f32,
    pub h: f32,
    pub rot: f32,
    pub fit: Fit,
}

impl Placement {
    pub fn of(l: &lightair_model::Layout) -> Placement {
        Placement {
            cx: l.x + l.width / 2.0,
            cy: l.y + l.height / 2.0,
            w: l.width,
            h: l.height,
            rot: l.rotation,
            fit: l.fit,
        }
    }
}

/// A point of the avatar's picture (`w`×`h`) on the canvas, where its item is.
pub fn on_canvas(host: &Placement, w: f32, h: f32, x: f32, y: f32) -> (f32, f32) {
    let (kx, ky) = match host.fit {
        Fit::Fit => {
            let k = (host.w / w).min(host.h / h);
            (k, k)
        }
        Fit::Fill => {
            let k = (host.w / w).max(host.h / h);
            (k, k)
        }
        Fit::Stretch => (host.w / w, host.h / h),
        Fit::Original => (1.0, 1.0),
    };
    let (lx, ly) = ((x - w / 2.0) * kx, (y - h / 2.0) * ky);
    let (s, c) = host.rot.to_radians().sin_cos();
    (host.cx + lx * c - ly * s, host.cy + lx * s + ly * c)
}
