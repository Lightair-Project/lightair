//! Canvas geometry for the editor (docs/07-scenes-layers-canvas.md, "Canvas"): rotated boxes,
//! hit-testing, and resize/crop computed in the layer's own frame. libobs turns a layer around
//! its box centre, clockwise for positive degrees (y points down).

use lightair_core::{Crop, Layout};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

pub const CORNERS: [Corner; 4] = [Corner::TopLeft, Corner::TopRight, Corner::BottomLeft, Corner::BottomRight];

/// Turns (x, y) by `deg` (clockwise on screen).
pub fn rotate((x, y): (f32, f32), deg: f32) -> (f32, f32) {
    let (s, c) = deg.to_radians().sin_cos();
    (x * c - y * s, x * s + y * c)
}

pub fn centre(l: &Layout) -> (f32, f32) {
    (l.x + l.width / 2.0, l.y + l.height / 2.0)
}

/// A point given relative to the box centre in the layer's frame, in canvas coordinates.
fn to_canvas(l: &Layout, local: (f32, f32)) -> (f32, f32) {
    let (cx, cy) = centre(l);
    let (x, y) = rotate(local, l.rotation);
    (cx + x, cy + y)
}

/// A canvas point relative to the box centre in the layer's frame.
fn to_local(l: &Layout, (x, y): (f32, f32)) -> (f32, f32) {
    let (cx, cy) = centre(l);
    rotate((x - cx, y - cy), -l.rotation)
}

pub fn corner(l: &Layout, c: Corner) -> (f32, f32) {
    let (hw, hh) = (l.width / 2.0, l.height / 2.0);
    let local = match c {
        Corner::TopLeft => (-hw, -hh),
        Corner::TopRight => (hw, -hh),
        Corner::BottomLeft => (-hw, hh),
        Corner::BottomRight => (hw, hh),
    };
    to_canvas(l, local)
}

/// The box outline, clockwise from the top-left corner.
pub fn outline(l: &Layout) -> [(f32, f32); 4] {
    [
        corner(l, Corner::TopLeft),
        corner(l, Corner::TopRight),
        corner(l, Corner::BottomRight),
        corner(l, Corner::BottomLeft),
    ]
}

/// The rotation handle: `offset` canvas units above the middle of the top edge.
pub fn rotate_handle(l: &Layout, offset: f32) -> (f32, f32) {
    to_canvas(l, (0.0, -l.height / 2.0 - offset))
}

pub fn contains(l: &Layout, p: (f32, f32)) -> bool {
    let (x, y) = to_local(l, p);
    x.abs() <= l.width / 2.0 && y.abs() <= l.height / 2.0
}

/// Places `edited` (a change to `orig` computed as if it weren't rotated) back under the
/// rotation: its centre moves by the same offset, turned with the layer.
fn reanchor(orig: &Layout, edited: Layout) -> Layout {
    let (ox, oy) = centre(orig);
    let (nx, ny) = centre(&edited);
    let (wx, wy) = rotate((nx - ox, ny - oy), orig.rotation);
    Layout { x: ox + wx - edited.width / 2.0, y: oy + wy - edited.height / 2.0, ..edited }
}

/// Dragging `c` by a canvas delta resizes the box, the opposite corner staying put.
/// `keep_aspect` keeps the width:height ratio, driven by the width.
pub fn resize(l: &Layout, c: Corner, delta: (f32, f32), keep_aspect: bool) -> Layout {
    let (dx, dy) = rotate(delta, -l.rotation);
    let min = 8.0;
    let (mut left, mut top, mut right, mut bottom) = (l.x, l.y, l.x + l.width, l.y + l.height);
    match c {
        Corner::TopLeft => (left, top) = (left + dx, top + dy),
        Corner::TopRight => (right, top) = (right + dx, top + dy),
        Corner::BottomLeft => (left, bottom) = (left + dx, bottom + dy),
        Corner::BottomRight => (right, bottom) = (right + dx, bottom + dy),
    }
    if keep_aspect {
        let h = (right - left).abs() * l.height / l.width.max(1.0);
        match c {
            Corner::TopLeft | Corner::TopRight => top = bottom - h,
            _ => bottom = top + h,
        }
    }
    let edited = Layout {
        x: left.min(right - min),
        y: top.min(bottom - min),
        width: (right - left).max(min),
        height: (bottom - top).max(min),
        ..*l
    };
    reanchor(l, edited)
}

/// ⌥-dragging `c` crops the source (`size` in source pixels) instead of scaling it: the box
/// edge follows the pointer and the picture keeps its scale.
pub fn crop(l: &Layout, c: Corner, size: (f32, f32), delta: (f32, f32)) -> Layout {
    let (dx, dy) = rotate(delta, -l.rotation);
    let (sw, sh) = size;
    let k = l.crop;
    // Source pixels per canvas pixel in the visible (already cropped) region.
    let sx = (sw - (k.left + k.right) as f32).max(1.0) / l.width.max(1.0);
    let sy = (sh - (k.top + k.bottom) as f32).max(1.0) / l.height.max(1.0);
    let min = 8.0;
    let edge = |orig: u32, other: u32, extent: f32, d: f32| -> u32 {
        (orig as f32 + d).clamp(0.0, (extent - other as f32 - min).max(0.0)).round() as u32
    };
    let left_side = matches!(c, Corner::TopLeft | Corner::BottomLeft);
    let top_side = matches!(c, Corner::TopLeft | Corner::TopRight);
    let left = if left_side { edge(k.left, k.right, sw, dx * sx) } else { k.left };
    let right = if left_side { k.right } else { edge(k.right, k.left, sw, -dx * sx) };
    let top = if top_side { edge(k.top, k.bottom, sh, dy * sy) } else { k.top };
    let bottom = if top_side { k.bottom } else { edge(k.bottom, k.top, sh, -dy * sy) };
    let (dl, dr) = ((left as f32 - k.left as f32) / sx, (right as f32 - k.right as f32) / sx);
    let (dt, db) = ((top as f32 - k.top as f32) / sy, (bottom as f32 - k.bottom as f32) / sy);
    let edited = Layout {
        x: l.x + dl,
        y: l.y + dt,
        width: l.width - dl - dr,
        height: l.height - dt - db,
        crop: Crop { left, top, right, bottom },
        ..*l
    };
    reanchor(l, edited)
}

/// An angle in (−180, 180].
pub fn normalise_degrees(a: f32) -> f32 {
    let a = a.rem_euclid(360.0);
    if a > 180.0 { a - 360.0 } else { a }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 0.01 && (a.1 - b.1).abs() < 0.01
    }

    #[test]
    fn rotated_corners_and_hits() {
        let l = Layout { rotation: 90.0, ..Layout::boxed(0.0, 0.0, 200.0, 100.0) };
        // Centre (100, 50); turned a quarter clockwise, the top-left corner goes to the top-right.
        assert!(close(corner(&l, Corner::TopLeft), (150.0, -50.0)), "{:?}", corner(&l, Corner::TopLeft));
        assert!(close(rotate_handle(&l, 20.0), (170.0, 50.0)));
        assert!(contains(&l, (100.0, -40.0)), "inside the turned box");
        assert!(!contains(&l, (10.0, 50.0)), "outside it, though inside the unturned one");
    }

    #[test]
    fn resize_keeps_the_opposite_corner_when_rotated() {
        for rotation in [0.0, 30.0, -120.0] {
            let l = Layout { rotation, ..Layout::boxed(100.0, 100.0, 400.0, 200.0) };
            let anchor = corner(&l, Corner::TopLeft);
            let r = resize(&l, Corner::BottomRight, rotate((50.0, 20.0), rotation), false);
            assert!((r.width - 450.0).abs() < 0.01 && (r.height - 220.0).abs() < 0.01, "{rotation}: {r:?}");
            assert!(close(corner(&r, Corner::TopLeft), anchor), "{rotation}: anchor moved");
        }
        let unrotated = resize(&Layout::boxed(0.0, 0.0, 100.0, 50.0), Corner::TopLeft, (10.0, 0.0), true);
        assert_eq!((unrotated.x, unrotated.width, unrotated.height), (10.0, 90.0, 45.0));
    }

    #[test]
    fn crop_keeps_the_picture_scale() {
        let l = Layout::boxed(100.0, 100.0, 960.0, 540.0);
        let c = crop(&l, Corner::TopLeft, (1920.0, 1080.0), (50.0, 25.0));
        assert_eq!((c.crop.left, c.crop.top, c.crop.right, c.crop.bottom), (100, 50, 0, 0));
        assert_eq!((c.x, c.y, c.width, c.height), (150.0, 125.0, 910.0, 515.0));
        let c2 = crop(&c, Corner::BottomRight, (1920.0, 1080.0), (-10.0, -10.0));
        assert_eq!((c2.crop.right, c2.crop.bottom), (20, 20));
        let back = crop(&c, Corner::TopLeft, (1920.0, 1080.0), (-500.0, -500.0));
        assert_eq!((back.crop.left, back.crop.top, back.x, back.y), (0, 0, 100.0, 100.0));
        // Rotated: the uncropped corner stays put.
        let r = Layout { rotation: 45.0, ..l };
        let anchor = corner(&r, Corner::BottomRight);
        let rc = crop(&r, Corner::TopLeft, (1920.0, 1080.0), rotate((50.0, 25.0), 45.0));
        assert_eq!((rc.crop.left, rc.crop.top), (100, 50));
        assert!(close(corner(&rc, Corner::BottomRight), anchor));
    }

    #[test]
    fn angles() {
        assert_eq!(normalise_degrees(270.0), -90.0);
        assert_eq!(normalise_degrees(-190.0), 170.0);
        assert_eq!(normalise_degrees(180.0), 180.0);
    }
}
