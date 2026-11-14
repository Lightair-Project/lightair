//! The player as a stage: the selected clip's picture outlined with handles; dragged to move
//! it, by a corner to scale it (snapping to the centre lines), or, in hide mode, a box drawn
//! over it to hide (pixelate) that area.

use super::*;
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
}

#[derive(Clone, Copy, PartialEq)]
enum StageKind {
    Move,
    Scale,
    Hide,
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
    fn to_result(&self, p: Point<Pixels>) -> Option<(f32, f32)> {
        let (o, k, off) = self.stage()?;
        Some(((f32::from(p.x - o.x) - off.0) / k, (f32::from(p.y - o.y) - off.1) / k))
    }

    /// The picture of `clip` in the shown frame.
    fn placed_of(&self, clip: Id) -> Option<Placed> {
        self.placed.iter().rev().find(|p| p.clip == clip).copied()
    }

    /// The topmost picture under a point (result pixels).
    fn picture_at(&self, p: (f32, f32)) -> Option<Placed> {
        self.placed.iter().rev().find(|q| (p.0 - q.center.0).abs() <= q.size.0 / 2.0 && (p.1 - q.center.1).abs() <= q.size.1 / 2.0).copied()
    }

    pub(super) fn stage_down(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        if self.playing.is_some() {
            return;
        }
        let (Some(p), Some((_, k, _)), Some(seq)) = (self.to_result(pos), self.stage(), self.seq().cloned()) else { return };
        let selected = self.selected.and_then(|c| self.placed_of(c));
        let start = |clip: Id, kind: StageKind| -> Option<StageDrag> {
            let c = seq.clip(clip)?;
            Some(StageDrag { clip, kind, from: p, start: c.transform_at(self.playhead), before: seq.clone(), to: p })
        };
        let drag = match selected {
            Some(s) if self.hiding => start(s.clip, StageKind::Hide),
            Some(s)
                if [(-1.0, -1.0), (1.0, -1.0), (-1.0, 1.0), (1.0, 1.0)]
                    .iter()
                    .any(|(dx, dy)| ((s.center.0 + dx * s.size.0 / 2.0 - p.0).hypot(s.center.1 + dy * s.size.1 / 2.0 - p.1)) * k <= HANDLE) =>
            {
                start(s.clip, StageKind::Scale)
            }
            _ => match self.picture_at(p) {
                Some(q) => {
                    // a locked track's clips stay put
                    let locked = seq.find(q.clip).is_some_and(|(t, _)| seq.tracks[t].locked);
                    self.selected = Some(q.clip);
                    if locked { None } else { start(q.clip, StageKind::Move) }
                }
                None => None,
            },
        };
        self.stage_drag = drag;
        cx.notify();
    }

    pub(super) fn stage_move(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        let (Some(d), Some(p), Some((_, k, _))) = (self.stage_drag.clone(), self.to_result(pos), self.stage()) else { return };
        let t = self.playhead;
        let size = self.seq().map_or((1920.0, 1080.0), |s| (s.size.0 as f32, s.size.1 as f32));
        let mut tr = d.start;
        self.guides = (false, false);
        match d.kind {
            StageKind::Hide => {
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
            c.set_transform_at(t, tr);
            self.pending.lock().unwrap().seq = Some(pr.sequence.clone());
        }
        self.request(t);
        cx.notify();
    }

    pub(super) fn stage_up(&mut self, cx: &mut Context<Self>) {
        let Some(d) = self.stage_drag.take() else { return };
        self.guides = (false, false);
        if d.kind == StageKind::Hide {
            self.hiding = false;
            // the box as fractions of the clip's picture
            if let Some(q) = self.placed_of(d.clip) {
                let (x0, y0) = (q.center.0 - q.size.0 / 2.0, q.center.1 - q.size.1 / 2.0);
                let fx = |x: f32| ((x - x0) / q.size.0).clamp(0.0, 1.0);
                let fy = |y: f32| ((y - y0) / q.size.1).clamp(0.0, 1.0);
                let (a, b) = ((fx(d.from.0.min(d.to.0)), fy(d.from.1.min(d.to.1))), (fx(d.from.0.max(d.to.0)), fy(d.from.1.max(d.to.1))));
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
        if let Some(q) = self.selected.and_then(|c| self.placed_of(c)) {
            let (x, y) = to_px(q.center.0 - q.size.0 / 2.0, q.center.1 - q.size.1 / 2.0);
            let (w, h) = (q.size.0 * k, q.size.1 * k);
            layer = layer.child(div().absolute().left(px(x)).top(px(y)).w(px(w)).h(px(h)).border_1().border_color(tok::accent_primary()));
            if !self.hiding {
                for (cx_, cy) in [(x, y), (x + w, y), (x, y + h), (x + w, y + h)] {
                    layer = layer.child(div().absolute().left(px(cx_ - 4.0)).top(px(cy - 4.0)).size(px(8.)).rounded(px(2.)).bg(rgb(0xFFFFFF)).border_1().border_color(tok::accent_primary()));
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
            layer = layer.child(div().absolute().left(px(x0)).top(px(y0)).w(px(x1 - x0)).h(px(y1 - y0)).bg(rgba(0xFFFFFF30)).border_1().border_color(rgb(0xFFFFFF)));
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
                        d.child(Button::new("ed-unhide").label("Show all again").icon(ui::icon("eye")).xsmall().ghost().on_click(cx.listener(move |this, _, _, cx| {
                            this.change(|s| if let Some(c) = s.clip_mut(cid) { c.hidden.clear() }, cx)
                        })))
                    }),
            )
            .child(hint(match n {
                0 => "Pixelates a part of the picture (a password, a face): draw a box over it in the player.".to_string(),
                n => format!("{n} hidden: shown dashed in the player. At most four."),
            }))
    }
}
