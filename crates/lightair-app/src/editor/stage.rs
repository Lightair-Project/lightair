//! The player as a stage: the selected clip's picture outlined with handles; dragged to move
//! it, by a corner to scale it (snapping to the centre lines), or, in hide mode, a box drawn
//! over it to hide (pixelate) that area. With a region zoom picked, the whole picture with the
//! zoom's region on it: dragged inside to move it, by a corner to size it, elsewhere to draw a
//! new one (always the picture's shape, 1× to 5×).

use super::*;
use lightair_render::edit::{Focus, Zoom};
use lightair_render::seqrender::Placed;

/// A drag on the player.
#[derive(Clone)]
pub(super) struct StageDrag {
    clip: Id,
    kind: StageKind,
    /// Where it began (result pixels), and the clip's transform then.
    from: (f32, f32),
    start: Transform,
    /// The sequence before (its undo step).
    before: Sequence,
    /// The box being drawn (hide mode), result pixels.
    to: (f32, f32),
    /// A zoom's region being set: the box it's measured in and how it's dragged.
    region: Option<RegionDrag>,
}

/// A zoom's region dragged on the player (result pixels).
#[derive(Clone, Copy)]
struct RegionDrag {
    zbox: (f32, f32, f32, f32),
    /// Moved, held this far from its top-left; or sized from this corner (a new one: from where
    /// the drag began).
    grab: Option<(f32, f32)>,
    anchor: (f32, f32),
    /// The region when the drag began.
    was: (f32, f32, f32, f32),
}

/// A zoom's region (left, top, width, height) in `zbox`.
fn region_of(zbox: (f32, f32, f32, f32), z: &Zoom) -> (f32, f32, f32, f32) {
    let (fx, fy) = match z.focus {
        Focus::Point { x, y } => (x, y),
        Focus::Pointer => (0.5, 0.5),
    };
    let s = z.scale.max(1.0);
    let (w, h) = (zbox.2 / s, zbox.3 / s);
    (zbox.0 + fx * zbox.2 - w / 2.0, zbox.1 + fy * zbox.3 - h / 2.0, w, h)
}

/// Where a region drag has it with the pointer at `p`: kept in the box, in its shape, 1× to 5×.
fn dragged_region(d: &RegionDrag, p: (f32, f32)) -> (f32, f32, f32, f32) {
    let (bx, by, bw, bh) = d.zbox;
    let r = match d.grab {
        Some(g) => (p.0 - g.0, p.1 - g.1, d.was.2, d.was.3),
        None => {
            let (dx, dy) = (p.0 - d.anchor.0, p.1 - d.anchor.1);
            let aspect = bh / bw.max(1.0);
            let w = dx.abs().max(dy.abs() / aspect).clamp(bw / 5.0, bw);
            let h = w * aspect;
            (
                if dx < 0.0 { d.anchor.0 - w } else { d.anchor.0 },
                if dy < 0.0 { d.anchor.1 - h } else { d.anchor.1 },
                w,
                h,
            )
        }
    };
    (r.0.clamp(bx, bx + bw - r.2), r.1.clamp(by, by + bh - r.3), r.2, r.3)
}

#[derive(Clone, Copy, PartialEq)]
enum StageKind {
    Move,
    Scale,
    Hide,
    Region,
}

/// Handle reach, in window pixels.
const HANDLE: f32 = 9.0;
/// Centre lines pull within this many window pixels.
const SNAP: f32 = 8.0;

impl Editor {
    /// How result pixels sit in the player: (its origin in the window, window pixels per
    /// result pixel, the frame's offset in the player).
    fn stage(&self) -> Option<(Point<Pixels>, f32, (f32, f32))> {
        let b = (*self.player_bounds.borrow())?;
        let (w, h) = self.seq().map(|s| (s.size.0 as f32, s.size.1 as f32))?;
        let (bw, bh) = (f32::from(b.size.width), f32::from(b.size.height));
        let k = (bw / w).min(bh / h);
        Some((b.origin, k, ((bw - w * k) / 2.0, (bh - h * k) / 2.0)))
    }

    /// A window point in result pixels.
    pub(super) fn to_result(&self, p: Point<Pixels>) -> Option<(f32, f32)> {
        let (o, k, off) = self.stage()?;
        Some(((f32::from(p.x - o.x) - off.0) / k, (f32::from(p.y - o.y) - off.1) / k))
    }

    /// The picture of `clip` in the shown frame.
    fn placed_of(&self, clip: Id) -> Option<Placed> {
        self.placed.iter().rev().find(|p| p.clip == clip).copied()
    }

    /// The topmost picture under a point (result pixels).
    pub(super) fn picture_at(&self, p: (f32, f32)) -> Option<Placed> {
        self.placed
            .iter()
            .rev()
            .find(|q| (p.0 - q.center.0).abs() <= q.size.0 / 2.0 && (p.1 - q.center.1).abs() <= q.size.1 / 2.0)
            .copied()
    }

    pub(super) fn stage_down(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        if self.playing.is_some() {
            return;
        }
        let (Some(p), Some((_, k, _)), Some(seq)) = (self.to_result(pos), self.stage(), self.seq().cloned()) else {
            return;
        };
        // a zoom's region: moved, sized or drawn anew
        if let Some((_, z)) = self.region_zoom()
            && let Some(zbox) = self.zoom_box()
        {
            let r = region_of(zbox, &z);
            let corners = [(r.0, r.1), (r.0 + r.2, r.1), (r.0, r.1 + r.3), (r.0 + r.2, r.1 + r.3)];
            let reach = HANDLE * 2.0 / k.max(0.01);
            let (grab, anchor) = if let Some(i) = corners.iter().position(|c| (c.0 - p.0).hypot(c.1 - p.1) <= reach) {
                (None, corners[3 - i])
            } else if p.0 >= r.0 && p.0 <= r.0 + r.2 && p.1 >= r.1 && p.1 <= r.1 + r.3 {
                (Some((p.0 - r.0, p.1 - r.1)), p)
            } else {
                (None, p)
            };
            let clip = Id::nil();
            let start = Default::default();
            self.stage_drag = Some(StageDrag {
                clip,
                kind: StageKind::Region,
                from: p,
                start,
                before: seq,
                to: p,
                region: Some(RegionDrag { zbox, grab, anchor, was: r }),
            });
            cx.notify();
            return;
        }
        let selected = self.selected.and_then(|c| self.placed_of(c));
        let playhead = self.playhead;
        let start = |clip: Id, kind: StageKind| -> Option<StageDrag> {
            let c = seq.clip(clip)?;
            Some(StageDrag {
                clip,
                kind,
                from: p,
                start: c.transform_at(playhead),
                before: seq.clone(),
                to: p,
                region: None,
            })
        };
        let drag = match selected {
            Some(s) if self.hiding => start(s.clip, StageKind::Hide),
            Some(s)
                if [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)].iter().any(|(dx, dy)| {
                    ((s.center.0 + dx * s.size.0 / 2.0 - p.0).hypot(s.center.1 + dy * s.size.1 / 2.0 - p.1)) * k
                        <= HANDLE
                }) =>
            {
                start(s.clip, StageKind::Scale)
            }
            _ => match self.picture_at(p) {
                Some(q) => {
                    // a locked track's clips stay put
                    let locked = seq.find(q.clip).is_some_and(|(t, _)| seq.tracks[t].locked);
                    self.pick_only(Some(q.clip));
                    if locked { None } else { start(q.clip, StageKind::Move) }
                }
                None => None,
            },
        };
        self.stage_drag = drag;
        cx.notify();
    }

    pub(super) fn stage_move(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let (Some(d), Some(p), Some((_, k, _))) = (self.stage_drag.clone(), self.to_result(pos), self.stage()) else {
            return;
        };
        let t = self.playhead;
        let size = self.seq().map_or((1920.0, 1080.0), |s| (s.size.0 as f32, s.size.1 as f32));
        let mut tr = d.start;
        self.guides = (false, false);
        match d.kind {
            StageKind::Hide | StageKind::Region => {
                if let Some(s) = &mut self.stage_drag {
                    s.to = p;
                }
                cx.notify();
                return;
            }
            StageKind::Move => {
                tr.x = d.start.x + p.0 - d.from.0;
                tr.y = d.start.y + p.1 - d.from.1;
                // the centre lines pull
                if tr.x.abs() * k < SNAP {
                    tr.x = 0.0;
                    self.guides.0 = true;
                }
                if tr.y.abs() * k < SNAP {
                    tr.y = 0.0;
                    self.guides.1 = true;
                }
            }
            StageKind::Scale => {
                let centre = (size.0 / 2.0 + d.start.x, size.1 / 2.0 + d.start.y);
                let was = (d.from.0 - centre.0).hypot(d.from.1 - centre.1).max(1.0);
                let now = (p.0 - centre.0).hypot(p.1 - centre.1);
                tr.scale = (d.start.scale * now / was).clamp(0.05, 10.0);
            }
        }
        if let Some(pr) = &mut self.project
            && let Some(c) = pr.sequence.clip_mut(d.clip)
        {
            // each value that changed, as a property: its key at the playhead when it's keyed
            let now = c.transform_at(t);
            for (p, a, b) in [("x", now.x, tr.x), ("y", now.y, tr.y), ("scale", now.scale, tr.scale)] {
                if a != b {
                    kit::set_prop(c, p, t, b);
                }
            }
            self.pending.lock().unwrap().seq = Some(pr.sequence.clone());
        }
        self.request(t);
        cx.notify();
    }

    pub(super) fn stage_up(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.stage_drag.take() else { return };
        self.guides = (false, false);
        if let Some(rd) = d.region {
            // a tap without a drag leaves it as it was
            if (d.to.0 - d.from.0).hypot(d.to.1 - d.from.1) > 2.0 {
                let r = dragged_region(&rd, d.to);
                let (bx, by, bw, bh) = rd.zbox;
                let scale = ((bw / r.2) * 10.0).round() / 10.0;
                let (fx, fy) = ((r.0 + r.2 / 2.0 - bx) / bw, (r.1 + r.3 / 2.0 - by) / bh);
                self.change_zoom(
                    |z| {
                        z.scale = scale.clamp(1.0, 5.0);
                        z.focus = Focus::Point { x: fx.clamp(0.0, 1.0), y: fy.clamp(0.0, 1.0) };
                    },
                    cx,
                );
            }
            cx.notify();
            return;
        }
        if d.kind == StageKind::Hide {
            self.hiding = false;
            // the box as fractions of the clip's picture
            if let Some(q) = self.placed_of(d.clip) {
                let (x0, y0) = (q.center.0 - q.size.0 / 2.0, q.center.1 - q.size.1 / 2.0);
                let fx = |x: f32| ((x - x0) / q.size.0).clamp(0.0, 1.0);
                let fy = |y: f32| ((y - y0) / q.size.1).clamp(0.0, 1.0);
                let (a, b) = (
                    (fx(d.from.0.min(d.to.0)), fy(d.from.1.min(d.to.1))),
                    (fx(d.from.0.max(d.to.0)), fy(d.from.1.max(d.to.1))),
                );
                if b.0 - a.0 > 0.01 && b.1 - a.1 > 0.01 {
                    self.change(
                        |s| {
                            if let Some(c) = s.clip_mut(d.clip) {
                                c.hidden.truncate(3);
                                c.hidden.push((a.0, a.1, b.0 - a.0, b.1 - a.1));
                            }
                        },
                        cx,
                    );
                }
            }
            cx.notify();
            return;
        }
        if let Some(p) = &self.project
            && p.sequence != d.before
        {
            self.undo.push(d.before);
            self.redo.clear();
        }
        self.settle(cx);
    }

    /// The selected picture's outline and handles, the centre lines while they pull, the box
    /// being drawn to hide.
    pub(super) fn stage_overlay(&self) -> Option<AnyElement> {
        if self.playing.is_some() {
            return None;
        }
        let (o, k, off) = self.stage()?;
        let b = (*self.player_bounds.borrow())?;
        let _ = o;
        let mut layer = div().absolute().top_0().left_0().size_full();
        let to_px = |x: f32, y: f32| (off.0 + x * k, off.1 + y * k);
        // a zoom's region on the whole picture: the rest dimmed, the region framed
        if let Some((_, z)) = self.region_zoom()
            && let Some(zbox) = self.zoom_box()
        {
            let r = match self.stage_drag.as_ref().and_then(|d| d.region.map(|rd| (rd, d.to, d.from))) {
                Some((rd, to, from)) if (to.0 - from.0).hypot(to.1 - from.1) > 2.0 => dragged_region(&rd, to),
                _ => region_of(zbox, &z),
            };
            let (bx, by) = to_px(zbox.0, zbox.1);
            let (bw, bh) = (zbox.2 * k, zbox.3 * k);
            let (x, y) = to_px(r.0, r.1);
            let (w, h) = (r.2 * k, r.3 * k);
            let dim = |l: f32, t: f32, w: f32, h: f32| {
                div().absolute().left(px(l)).top(px(t)).w(px(w.max(0.0))).h(px(h.max(0.0))).bg(rgba(0x00000080))
            };
            layer = layer
                .child(dim(bx, by, bw, y - by))
                .child(dim(bx, y + h, bw, by + bh - y - h))
                .child(dim(bx, y, x - bx, h))
                .child(dim(x + w, y, bx + bw - x - w, h))
                .child(
                    div()
                        .absolute()
                        .left(px(x))
                        .top(px(y))
                        .w(px(w))
                        .h(px(h))
                        .border_2()
                        .border_color(rgb(0xFFFFFF))
                        .cursor(CursorStyle::OpenHand),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(x + 6.0))
                        .top(px(y + 6.0))
                        .px_1p5()
                        .rounded_full()
                        .bg(rgba(0x141414C0))
                        .text_size(px(11.))
                        .text_color(rgb(0xFFFFFF))
                        .child(format!("{:.1}×", zbox.2 / r.2.max(1.0))),
                );
            for (i, (cx_, cy)) in [(x, y), (x + w, y), (x, y + h), (x + w, y + h)].into_iter().enumerate() {
                let cursor = if i == 0 || i == 3 {
                    CursorStyle::ResizeUpLeftDownRight
                } else {
                    CursorStyle::ResizeUpRightDownLeft
                };
                layer = layer.child(
                    div()
                        .absolute()
                        .left(px(cx_ - 8.0))
                        .top(px(cy - 8.0))
                        .size(px(16.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor(cursor)
                        .child(div().size(px(8.)).rounded(px(2.)).bg(rgb(0xFFFFFF))),
                );
            }
            return Some(layer.into_any_element());
        }
        if let Some(q) = self.selected.and_then(|c| self.placed_of(c)) {
            let (x, y) = to_px(q.center.0 - q.size.0 / 2.0, q.center.1 - q.size.1 / 2.0);
            let (w, h) = (q.size.0 * k, q.size.1 * k);
            // grabbed to move it (an open hand; closed while it's dragged)
            let moving = self.stage_drag.as_ref().is_some_and(|d| d.kind == StageKind::Move);
            layer = layer.child(
                div()
                    .absolute()
                    .left(px(x))
                    .top(px(y))
                    .w(px(w))
                    .h(px(h))
                    .border_1()
                    .border_color(tok::accent_primary())
                    .when(!self.hiding, |d| {
                        d.cursor(if moving { CursorStyle::ClosedHand } else { CursorStyle::OpenHand })
                    }),
            );
            if !self.hiding {
                // corners scale it: the cursor along their diagonal
                let corners = [
                    (x, y, CursorStyle::ResizeUpLeftDownRight),
                    (x + w, y, CursorStyle::ResizeUpRightDownLeft),
                    (x, y + h, CursorStyle::ResizeUpRightDownLeft),
                    (x + w, y + h, CursorStyle::ResizeUpLeftDownRight),
                ];
                for (cx_, cy, cursor) in corners {
                    layer = layer.child(
                        div()
                            .absolute()
                            .left(px(cx_ - 8.0))
                            .top(px(cy - 8.0))
                            .size(px(16.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor(cursor)
                            .child(
                                div()
                                    .size(px(8.))
                                    .rounded(px(2.))
                                    .bg(rgb(0xFFFFFF))
                                    .border_1()
                                    .border_color(tok::accent_primary()),
                            ),
                    );
                }
            }
            // its hidden areas
            if let Some(c) = self.seq().and_then(|s| s.clip(q.clip)) {
                for m in &c.hidden {
                    layer = layer.child(
                        div()
                            .absolute()
                            .left(px(x + m.0 * w))
                            .top(px(y + m.1 * h))
                            .w(px(m.2 * w))
                            .h(px(m.3 * h))
                            .border_1()
                            .border_dashed()
                            .border_color(rgba(0xFFFFFFC0)),
                    );
                }
            }
        }
        if let Some(d) = self.stage_drag.as_ref().filter(|d| d.kind == StageKind::Hide) {
            let (x0, y0) = to_px(d.from.0.min(d.to.0), d.from.1.min(d.to.1));
            let (x1, y1) = to_px(d.from.0.max(d.to.0), d.from.1.max(d.to.1));
            layer = layer.child(
                div()
                    .absolute()
                    .left(px(x0))
                    .top(px(y0))
                    .w(px(x1 - x0))
                    .h(px(y1 - y0))
                    .bg(rgba(0xFFFFFF30))
                    .border_1()
                    .border_color(rgb(0xFFFFFF)),
            );
        }
        let (bw, bh) = (f32::from(b.size.width), f32::from(b.size.height));
        if self.guides.0 {
            layer = layer.child(div().absolute().top_0().left(px(bw / 2.0)).w(px(1.)).h(px(bh)).bg(rgb(0xF472B6)));
        }
        if self.guides.1 {
            layer = layer.child(div().absolute().left_0().top(px(bh / 2.0)).h(px(1.)).w(px(bw)).bg(rgb(0xF472B6)));
        }
        Some(layer.into_any_element())
    }

    /// The selected clip's hidden areas, in the inspector.
    pub(super) fn hidden_section(&self, c: &lightair_render::sequence::Clip, body: Div, cx: &mut Context<Self>) -> Div {
        let cid = c.id;
        let n = c.hidden.len();
        body.child(label("HIDDEN AREAS"))
            .child(
                div()
                    .flex()
                    .gap_1()
                    .child(
                        Button::new("ed-hide")
                            .label(if self.hiding { "Drag over the area…" } else { "Hide an area" })
                            .icon(ui::icon("eye-off"))
                            .xsmall()
                            .map(|b| if self.hiding { b.primary() } else { b.outline() })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.hiding = !this.hiding;
                                cx.notify();
                            })),
                    )
                    .when(n > 0, |d| {
                        d.child(
                            Button::new("ed-unhide")
                                .label("Show all again")
                                .icon(ui::icon("eye"))
                                .xsmall()
                                .ghost()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.change(
                                        |s| {
                                            if let Some(c) = s.clip_mut(cid) {
                                                c.hidden.clear()
                                            }
                                        },
                                        cx,
                                    )
                                })),
                        )
                    }),
            )
            .child(hint(match n {
                0 => "Pixelates a part of the picture (a password, a face): draw a box over it in the player."
                    .to_string(),
                n => format!("{n} hidden: shown dashed in the player. At most four."),
            }))
    }
}
