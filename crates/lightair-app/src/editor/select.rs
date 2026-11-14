//! Picking clips on the timeline: a click picks one, ⌘-click adds or takes one away, ⇧-click
//! picks the range from the last one picked, and a box dragged over the tracks (a marquee)
//! picks what it touches (adding with ⌘ or ⇧). What's picked moves and is deleted together.

use super::*;

/// The box being dragged (window points), and what was picked before it (kept when adding).
#[derive(Clone)]
pub(super) struct Marquee {
    from: Point<Pixels>,
    to: Point<Pixels>,
    base: Vec<Id>,
}

impl Editor {
    /// What's picked: the clips, the inspector's one last.
    pub(super) fn picked_clips(&self) -> Vec<Id> {
        let mut v = self.picked.clone();
        if let Some(s) = self.selected
            && !v.contains(&s)
        {
            v.push(s);
        }
        v
    }

    pub(super) fn is_picked(&self, clip: Id) -> bool {
        self.selected == Some(clip) || self.picked.contains(&clip)
    }

    /// Picks only `clip` (or nothing).
    pub(super) fn pick_only(&mut self, clip: Option<Id>) {
        self.picked = clip.into_iter().collect();
        self.selected = clip;
    }

    /// A click on a clip with ⌘ (add or take away) or ⇧ (the range from the last picked).
    pub(super) fn pick_click(&mut self, clip: Id, range: bool, cx: &mut Context<Self>) {
        let Some(seq) = self.seq().cloned() else { return };
        match (range, self.selected) {
            (true, Some(anchor)) if anchor != clip => {
                let (Some((ta, _)), Some((tb, _))) = (seq.find(anchor), seq.find(clip)) else { return };
                let (a, b) = (seq.clip(anchor).cloned(), seq.clip(clip).cloned());
                let (Some(a), Some(b)) = (a, b) else { return };
                let (from, to) = (a.start_ms.min(b.start_ms), a.end_ms().max(b.end_ms()));
                let rows = ta.min(tb)..=ta.max(tb);
                let mut picked = self.picked.clone();
                for (ti, track) in seq.tracks.iter().enumerate().filter(|(i, _)| rows.contains(i)) {
                    for c in track.clips.iter().filter(|c| c.start_ms < to && c.end_ms() > from) {
                        if !picked.contains(&c.id) {
                            picked.push(c.id);
                        }
                    }
                    let _ = ti;
                }
                self.picked = picked;
                self.selected = Some(clip);
            }
            _ => {
                let mut picked = self.picked_clips();
                match picked.iter().position(|c| *c == clip) {
                    Some(i) => {
                        picked.remove(i);
                        self.selected = picked.last().copied();
                    }
                    None => {
                        picked.push(clip);
                        self.selected = Some(clip);
                    }
                }
                self.picked = picked;
            }
        }
        cx.notify();
    }

    /// Every clip, picked (⌘A).
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        let all: Vec<Id> = self.seq().map(|s| s.tracks.iter().flat_map(|t| t.clips.iter().map(|c| c.id)).collect()).unwrap_or_default();
        self.selected = all.last().copied();
        self.picked = all;
        cx.notify();
    }

    pub(super) fn begin_marquee(&mut self, at: Point<Pixels>, adding: bool) {
        let base = if adding { self.picked_clips() } else { Vec::new() };
        if !adding {
            self.pick_only(None);
        }
        self.marquee = Some(Marquee { from: at, to: at, base });
    }

    /// The box grown to `to`: what it touches, picked.
    pub(super) fn marquee_to(&mut self, to: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(m) = &mut self.marquee else { return };
        m.to = to;
        let (from, base) = (m.from, m.base.clone());
        let (Some(b), Some(seq)) = (*self.timeline_bounds.borrow(), self.seq().cloned()) else { return };
        let (x0, x1) = (f32::from(from.x.min(to.x)), f32::from(from.x.max(to.x)));
        let (y0, y1) = (f32::from(from.y.min(to.y)), f32::from(from.y.max(to.y)));
        // a click without a drag picks nothing
        if x1 - x0 < 3.0 && y1 - y0 < 3.0 {
            return;
        }
        let left = f32::from(b.origin.x) + HEADER_W;
        let mut picked = base;
        for (ti, track) in seq.tracks.iter().enumerate() {
            let Some(top) = self.track_top(ti) else { continue };
            let h = track_h(track);
            if top + h - 3.0 < y0 || top + 3.0 > y1 {
                continue;
            }
            for c in &track.clips {
                let (cx0, cx1) = (left + self.x_of(c.start_ms), left + self.x_of(c.end_ms()));
                if cx1 >= x0 && cx0 <= x1 && !picked.contains(&c.id) {
                    picked.push(c.id);
                }
            }
        }
        self.selected = picked.last().copied();
        self.picked = picked;
        cx.notify();
    }

    pub(super) fn end_marquee(&mut self, cx: &mut Context<Self>) {
        if self.marquee.take().is_some() {
            cx.notify();
        }
    }

    /// The box, drawn over the timeline.
    pub(super) fn marquee_overlay(&self) -> Option<AnyElement> {
        let m = self.marquee.as_ref()?;
        let b = (*self.timeline_bounds.borrow())?;
        let (x0, x1) = (f32::from(m.from.x.min(m.to.x) - b.origin.x), f32::from(m.from.x.max(m.to.x) - b.origin.x));
        let (y0, y1) = (f32::from(m.from.y.min(m.to.y) - b.origin.y), f32::from(m.from.y.max(m.to.y) - b.origin.y));
        if x1 - x0 < 3.0 && y1 - y0 < 3.0 {
            return None;
        }
        Some(
            div()
                .absolute()
                .left(px(x0))
                .top(px(y0))
                .w(px(x1 - x0))
                .h(px(y1 - y0))
                .bg(rgba(0x3B82F626))
                .border_1()
                .border_color(rgba(0x60A5FAE0))
                .into_any_element(),
        )
    }

    /// Moves every picked clip by the grabbed one's move (`start` its new start), from where
    /// they were when the drag began; nothing moves if one can't.
    pub(super) fn move_picked(&mut self, grabbed: Id, start: u64) -> bool {
        let Some(before) = self.drag_before.clone() else { return false };
        let picked = self.picked_clips();
        if picked.len() < 2 || !picked.contains(&grabbed) {
            return false;
        }
        let Some(was) = before.clip(grabbed).map(|c| c.start_ms) else { return false };
        let delta = start as i64 - was as i64;
        let Some(p) = &mut self.project else { return false };
        let mut s = before.clone();
        let mut moves: Vec<(Id, usize, u64)> = picked
            .iter()
            .filter_map(|id| {
                let (t, _) = before.find(*id)?;
                let c = before.clip(*id)?;
                Some((*id, t, (c.start_ms as i64 + delta).max(0) as u64))
            })
            .collect();
        // the ones ahead first, so none runs into another that's moving
        moves.sort_by_key(|(id, _, _)| before.clip(*id).map_or(0, |c| c.start_ms));
        if delta > 0 {
            moves.reverse();
        }
        let ok = moves.iter().all(|(id, t, at)| s.move_clip(*id, *t, *at));
        p.sequence = if ok { s } else { before };
        true
    }
}
