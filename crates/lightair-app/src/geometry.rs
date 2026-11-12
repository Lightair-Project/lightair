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

/// The axis-aligned bounds of a (possibly rotated) box: (left, top, right, bottom).
pub fn bounds(l: &Layout) -> (f32, f32, f32, f32) {
    let o = outline(l);
    let xs = o.map(|p| p.0);
    let ys = o.map(|p| p.1);
    let min = |v: [f32; 4]| v.into_iter().fold(f32::INFINITY, f32::min);
    let max = |v: [f32; 4]| v.into_iter().fold(f32::NEG_INFINITY, f32::max);
    (min(xs), min(ys), max(xs), max(ys))
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Align {
    Left,
    Centre,
    Right,
    Top,
    Middle,
    Bottom,
}

/// Lines layers up by their visible bounds, against the bounds of them all.
pub fn align(layouts: &[Layout], how: Align) -> Vec<Layout> {
    let all: Vec<_> = layouts.iter().map(bounds).collect();
    let left = all.iter().map(|b| b.0).fold(f32::INFINITY, f32::min);
    let top = all.iter().map(|b| b.1).fold(f32::INFINITY, f32::min);
    let right = all.iter().map(|b| b.2).fold(f32::NEG_INFINITY, f32::max);
    let bottom = all.iter().map(|b| b.3).fold(f32::NEG_INFINITY, f32::max);
    layouts
        .iter()
        .zip(all)
        .map(|(l, b)| {
            let (dx, dy) = match how {
                Align::Left => (left - b.0, 0.0),
                Align::Right => (right - b.2, 0.0),
                Align::Centre => ((left + right) / 2.0 - (b.0 + b.2) / 2.0, 0.0),
                Align::Top => (0.0, top - b.1),
                Align::Bottom => (0.0, bottom - b.3),
                Align::Middle => (0.0, (top + bottom) / 2.0 - (b.1 + b.3) / 2.0),
            };
            Layout { x: l.x + dx, y: l.y + dy, ..*l }
        })
        .collect()
}

/// Spaces layers evenly between the outermost two (by visible bounds): equal gaps, along x
/// when `horizontal`, else y. Needs three or more; fewer are returned unchanged.
pub fn distribute(layouts: &[Layout], horizontal: bool) -> Vec<Layout> {
    if layouts.len() < 3 {
        return layouts.to_vec();
    }
    let span = |l: &Layout| {
        let b = bounds(l);
        if horizontal { (b.0, b.2) } else { (b.1, b.3) }
    };
    let mut order: Vec<usize> = (0..layouts.len()).collect();
    order.sort_by(|a, b| span(&layouts[*a]).0.total_cmp(&span(&layouts[*b]).0));
    let first = span(&layouts[order[0]]).0;
    let last = span(&layouts[*order.last().unwrap()]).1;
    let total: f32 = layouts.iter().map(|l| span(l).1 - span(l).0).sum();
    let gap = (last - first - total) / (layouts.len() - 1) as f32;
    let mut out = layouts.to_vec();
    let mut at = first;
    for i in order {
        let (start, end) = span(&layouts[i]);
        let d = at - start;
        if horizontal {
            out[i].x += d;
        } else {
            out[i].y += d;
        }
        at += end - start + gap;
    }
    out
}

/// Whether a layer's visible bounds overlap the rectangle between two points.
pub fn touches(l: &Layout, a: (f32, f32), b: (f32, f32)) -> bool {
    let (x0, x1) = (a.0.min(b.0), a.0.max(b.0));
    let (y0, y1) = (a.1.min(b.1), a.1.max(b.1));
    let (l0, t0, r0, b0) = bounds(l);
    l0 <= x1 && r0 >= x0 && t0 <= y1 && b0 >= y0
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

pub const EDGES: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

/// The middle of an edge, in canvas coordinates.
pub fn edge_middle(l: &Layout, e: Edge) -> (f32, f32) {
    let (hw, hh) = (l.width / 2.0, l.height / 2.0);
    let local = match e {
        Edge::Left => (-hw, 0.0),
        Edge::Right => (hw, 0.0),
        Edge::Top => (0.0, -hh),
        Edge::Bottom => (0.0, hh),
    };
    to_canvas(l, local)
}

/// Dragging one edge crops that side only (the pointer's movement along the edge is ignored).
pub fn crop_edge(l: &Layout, e: Edge, size: (f32, f32), delta: (f32, f32)) -> Layout {
    let (dx, dy) = rotate(delta, -l.rotation);
    let (corner, local) = match e {
        Edge::Left => (Corner::TopLeft, (dx, 0.0)),
        Edge::Right => (Corner::TopRight, (dx, 0.0)),
        Edge::Top => (Corner::TopLeft, (0.0, dy)),
        Edge::Bottom => (Corner::BottomLeft, (0.0, dy)),
    };
    crop(l, corner, size, rotate(local, l.rotation))
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
    fn alignment_uses_visible_bounds() {
        let a = Layout::boxed(10.0, 10.0, 100.0, 50.0);
        let b = Layout::boxed(200.0, 100.0, 50.0, 50.0);
        let left = align(&[a, b], Align::Left);
        assert_eq!((left[0].x, left[1].x), (10.0, 10.0));
        let bottom = align(&[a, b], Align::Bottom);
        assert_eq!((bottom[0].y, bottom[1].y), (100.0, 100.0));
        let centre = align(&[a, b], Align::Centre);
        assert_eq!((centre[0].x + 50.0, centre[1].x + 25.0), (130.0, 130.0));
        // A layer turned a quarter shows 50 wide: it aligns by that.
        let turned = Layout { rotation: 90.0, ..Layout::boxed(300.0, 0.0, 100.0, 50.0) };
        let r = align(&[a, turned], Align::Left);
        assert!((bounds(&r[1]).0 - 10.0).abs() < 0.01);
    }

    #[test]
    fn distribute_makes_equal_gaps() {
        let ls = [
            Layout::boxed(0.0, 0.0, 100.0, 10.0),
            Layout::boxed(500.0, 0.0, 100.0, 10.0),
            Layout::boxed(130.0, 0.0, 50.0, 10.0),
        ];
        let d = distribute(&ls, true);
        // Span 0..600, widths 250: two gaps of 175.
        assert_eq!((d[0].x, d[2].x, d[1].x), (0.0, 275.0, 500.0));
        assert_eq!(distribute(&ls[..2], true), ls[..2].to_vec(), "two layers stay put");
    }

    #[test]
    fn marquee_touches() {
        let l = Layout::boxed(100.0, 100.0, 50.0, 50.0);
        assert!(touches(&l, (0.0, 0.0), (120.0, 120.0)));
        assert!(touches(&l, (200.0, 200.0), (140.0, 140.0)), "either drag direction");
        assert!(!touches(&l, (0.0, 0.0), (90.0, 300.0)));
    }

    #[test]
    fn edges_crop_one_side() {
        let l = Layout::boxed(100.0, 100.0, 960.0, 540.0);
        let c = crop_edge(&l, Edge::Right, (1920.0, 1080.0), (-50.0, 30.0));
        assert_eq!((c.crop.left, c.crop.top, c.crop.right, c.crop.bottom), (0, 0, 100, 0));
        assert_eq!((c.x, c.y, c.width, c.height), (100.0, 100.0, 910.0, 540.0));
        let t = crop_edge(&l, Edge::Top, (1920.0, 1080.0), (80.0, 20.0));
        assert_eq!((t.crop.top, t.y, t.height), (40, 120.0, 520.0));
        // Turned a quarter clockwise, the bottom edge is on the left of the screen: dragging it
        // right (inwards) crops the bottom.
        let r = Layout { rotation: 90.0, ..l };
        let b = crop_edge(&r, Edge::Bottom, (1920.0, 1080.0), (20.0, 0.0));
        assert_eq!((b.crop.bottom, b.crop.left, b.crop.right), (40, 0, 0));
        assert!(close(edge_middle(&b, Edge::Top), edge_middle(&r, Edge::Top)), "the opposite edge stays");
    }

    #[test]
    fn angles() {
        assert_eq!(normalise_degrees(270.0), -90.0);
        assert_eq!(normalise_degrees(-190.0), 170.0);
        assert_eq!(normalise_degrees(180.0), 180.0);
    }
}
