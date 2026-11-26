//! Picking Library items: a click picks one, ⌘-click adds or takes one away, ⇧-click picks the
//! run from the last one picked, and a box dragged over the cards picks what it touches (adding
//! with ⌘ or ⇧). What's picked is opened in the Editor, moved or put in the Trash together.

use super::*;

/// The box being dragged (window points), and what was picked before it (kept when adding).
pub(super) struct Marquee {
    from: Point<Pixels>,
    to: Point<Pixels>,
    base: Vec<(PathBuf, Id)>,
}

impl Library {
    /// A press on a card: what it picks, by its keys.
    pub(super) fn pick_card(&mut self, dir: &Path, id: Id, mods: Modifiers, cx: &mut Context<Self>) {
        let key = (dir.to_path_buf(), id);
        if mods.shift
            && let Some(anchor) = self.anchor.clone()
        {
            // the run between the last picked and this one, as the cards are shown
            let shown: Vec<(PathBuf, Id)> = self.shown(cx).into_iter().map(|(d, i)| (d, i.id)).collect();
            if let (Some(a), Some(b)) = (shown.iter().position(|k| *k == anchor), shown.iter().position(|k| *k == key)) {
                for k in &shown[a.min(b)..=a.max(b)] {
                    if !self.picked.contains(k) {
                        self.picked.push(k.clone());
                    }
                }
            }
        } else if mods.platform || mods.control {
            match self.picked.iter().position(|k| *k == key) {
                Some(i) => {
                    self.picked.remove(i);
                }
                None => self.picked.push(key.clone()),
            }
            self.anchor = Some(key);
        } else {
            self.picked = vec![key.clone()];
            self.anchor = Some(key);
        }
        cx.notify();
    }

    pub(super) fn is_picked(&self, dir: &Path, id: Id) -> bool {
        self.picked.iter().any(|(d, i)| d == dir && *i == id)
    }

    /// A press on the empty space between cards: a box to pick with.
    pub(super) fn begin_marquee(&mut self, at: Point<Pixels>, mods: Modifiers) {
        let adding = mods.platform || mods.control || mods.shift;
        let base = if adding { self.picked.clone() } else { Vec::new() };
        if !adding {
            self.picked.clear();
        }
        self.marquee = Some(Marquee { from: at, to: at, base });
    }

    pub(super) fn marquee_to(&mut self, to: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(m) = &mut self.marquee else { return };
        m.to = to;
        let (x0, x1) = (m.from.x.min(to.x), m.from.x.max(to.x));
        let (y0, y1) = (m.from.y.min(to.y), m.from.y.max(to.y));
        if f32::from(x1 - x0) < 3.0 && f32::from(y1 - y0) < 3.0 {
            return;
        }
        let mut picked = m.base.clone();
        for (key, b) in self.card_bounds.borrow().iter() {
            let touches = b.origin.x <= x1 && b.origin.x + b.size.width >= x0 && b.origin.y <= y1 && b.origin.y + b.size.height >= y0;
            if touches && !picked.contains(key) {
                picked.push(key.clone());
            }
        }
        self.picked = picked;
        cx.notify();
    }

    pub(super) fn end_marquee(&mut self, cx: &mut Context<Self>) {
        if self.marquee.take().is_some() {
            cx.notify();
        }
    }

    /// The box, over the cards (positions from the body's top-left).
    pub(super) fn marquee_overlay(&self) -> Option<AnyElement> {
        let m = self.marquee.as_ref()?;
        let b = (*self.body_bounds.borrow())?;
        let (x0, x1) = (f32::from(m.from.x.min(m.to.x) - b.origin.x), f32::from(m.from.x.max(m.to.x) - b.origin.x));
        let (y0, y1) = (f32::from(m.from.y.min(m.to.y) - b.origin.y), f32::from(m.from.y.max(m.to.y) - b.origin.y));
        if x1 - x0 < 3.0 && y1 - y0 < 3.0 {
            return None;
        }
        Some(div().absolute().left(px(x0)).top(px(y0)).w(px(x1 - x0)).h(px(y1 - y0)).bg(rgba(0x3B82F626)).border_1().border_color(rgba(0x60A5FAE0)).into_any_element())
    }

    /// What can be done with several picked: a bar above the cards.
    pub(super) fn picked_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.picked.len() < 2 {
            return None;
        }
        let n = self.picked.len();
        Some(
            div()
                .flex()
                .items_center()
                .gap_2()
                .px_4()
                .py_1p5()
                .border_b_1()
                .border_color(tok::border())
                .bg(tok::bg_panel())
                .text_size(px(12.))
                .child(div().font_weight(FontWeight::SEMIBOLD).child(format!("{n} selected")))
                .child(div().flex_1())
                .child(Button::new("lib-many-edit").label("Open in Editor").icon(ui::icon("scissors")).xsmall().outline().on_click(cx.listener(|this, _, _, cx| {
                    this.open_many = Some(this.picked.iter().map(|(_, id)| *id).collect());
                    cx.notify();
                })))
                .child(Button::new("lib-many-trash").label(format!("Move to {}…", trash_name())).icon(ui::icon("trash")).xsmall().outline().on_click(cx.listener(|this, _, _, cx| {
                    this.confirm_trash = this.picked.clone();
                    cx.notify();
                })))
                .child(Button::new("lib-many-clear").label("Clear").xsmall().ghost().on_click(cx.listener(|this, _, _, cx| {
                    this.picked.clear();
                    cx.notify();
                })))
                .into_any_element(),
        )
    }
}

impl Library {
    /// For the screenshot hook: the first two cards picked.
    pub(crate) fn hook_pick(&mut self, cx: &mut Context<Self>) {
        self.reload(cx);
        let shown: Vec<(PathBuf, Id)> = self.shown(cx).into_iter().map(|(d, i)| (d, i.id)).take(2).collect();
        self.picked = shown;
        cx.notify();
    }

    /// For the check hook: the first card shown played in the Library's player.
    pub(crate) fn hook_play(&mut self, cx: &mut Context<Self>) {
        self.reload(cx);
        if let Some((dir, id)) = self.shown(cx).into_iter().map(|(d, i)| (d, i.id)).next() {
            self.play(&dir, id, cx);
        }
    }
}
