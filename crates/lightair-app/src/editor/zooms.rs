//! The zoom track, as Screen Studio has it: a lane under the tracks with each screen take's
//! zooms as blocks where they play. A press on the lane over a take adds one there; a block
//! is picked with a click, moved by dragging it and lengthened by its edges; the inspector sets
//! how far it zooms and where it looks (following the pointer, or a point). Zooms belong to the
//! take (source time), kept in the project so they're undone like anything else.

use super::*;
use lightair_render::edit::{Focus, Zoom};
use lightair_render::sequence::Clip;

pub(super) const ZOOM_H: f32 = 34.0;
/// A zoom at least this long (ms).
const SHORTEST: u64 = 300;
/// A new zoom's length (ms).
const NEW_LEN: u64 = 2_000;

/// Which part of a zoom block is dragged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ZoomPart {
    /// The whole block, held `grab` ms (source) after its start.
    Body { grab: u64 },
    Start,
    End,
}

/// A zoom block on the lane: its take, its place in the take's zooms, the clip it's shown
/// over, and where it is on the timeline.
#[derive(Clone, Copy)]
struct Block {
    media: Id,
    index: usize,
    clip: Id,
    zoom: Zoom,
    from: u64,
    to: u64,
}

/// A take's source time `s` on the timeline, through clip `c`.
fn on_timeline(c: &Clip, s: u64) -> u64 {
    c.start_ms + ((s.saturating_sub(c.in_ms)) as f64 / c.speed.max(0.05) as f64) as u64
}

/// Timeline time `t` in the take's source time, through clip `c` (held to its ends).
fn in_source(c: &Clip, t: u64) -> u64 {
    let t = t.clamp(c.start_ms, c.end_ms());
    (c.in_ms + ((t - c.start_ms) as f64 * c.speed.max(0.05) as f64) as u64).min(c.out_ms)
}

impl Editor {
    /// A take's zooms: as edited here, else its look's own (read once).
    pub(super) fn zooms_of(&self, m: &Media) -> Vec<Zoom> {
        if let Some(z) = &m.zooms {
            return z.clone();
        }
        self.zoom_files.borrow_mut().entry(m.id).or_insert_with(|| self.look_of(m).zooms).clone()
    }

    /// Where the tracks start, from the timeline's top: under the ruler and the zoom track.
    pub(super) fn tracks_top(&self) -> f32 {
        RULER_H + if self.has_takes() { ZOOM_H } else { 0.0 }
    }

    fn has_takes(&self) -> bool {
        self.seq().is_some_and(|s| s.tracks.iter().filter(|t| t.kind == TrackKind::Video).flat_map(|t| &t.clips).any(|c| c.content.is_none() && s.media(c.media).is_some_and(|m| m.take.is_some())))
    }

    /// The take clips (on video tracks) with their media.
    fn take_clips(&self) -> Vec<(Clip, Media)> {
        let Some(seq) = self.seq() else { return Vec::new() };
        seq.tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Video)
            .flat_map(|t| t.clips.iter())
            .filter_map(|c| {
                let m = seq.media(c.media)?;
                (m.take.is_some() && c.content.is_none()).then(|| (c.clone(), m.clone()))
            })
            .collect()
    }

    fn blocks(&self) -> Vec<Block> {
        let mut out = Vec::new();
        for (c, m) in self.take_clips() {
            for (index, z) in self.zooms_of(&m).into_iter().enumerate() {
                let (s0, s1) = (z.start_ms.max(c.in_ms), z.end_ms.min(c.out_ms));
                if s1 > s0 {
                    out.push(Block { media: m.id, index, clip: c.id, zoom: z, from: on_timeline(&c, s0), to: on_timeline(&c, s1) });
                }
            }
        }
        out
    }

    /// Changes a take's zooms (as edited here from now on), one undo step.
    fn set_zooms(&mut self, media: Id, f: impl FnOnce(&mut Vec<Zoom>), cx: &mut Context<Self>) {
        let Some(m) = self.seq().and_then(|s| s.media(media)).cloned() else { return };
        let mut zooms = self.zooms_of(&m);
        f(&mut zooms);
        self.change(|s| if let Some(m) = s.media.iter_mut().find(|m| m.id == media) { m.zooms = Some(zooms) }, cx);
    }

    pub(super) fn picked_zoom(&self) -> Option<(Media, usize, Zoom)> {
        let (media, i) = self.zoom_sel?;
        let m = self.seq()?.media(media)?.clone();
        let z = *self.zooms_of(&m).get(i)?;
        Some((m, i, z))
    }

    fn pick_zoom(&mut self, z: Option<(Id, usize)>, cx: &mut Context<Self>) {
        if z.is_some() {
            self.pick_only(None);
        }
        self.zoom_sel = z;
        cx.notify();
    }

    /// A zoom added at timeline time `t` (over a take there), picked.
    pub(super) fn add_zoom_at(&mut self, t: u64, cx: &mut Context<Self>) {
        let Some((c, m)) = self.take_clips().into_iter().find(|(c, _)| c.start_ms <= t && t < c.end_ms()) else {
            self.note = Some("Zooms go on screen recordings: put the playhead over a Studio take.".into());
            cx.notify();
            return;
        };
        let s = in_source(&c, t);
        let zooms = self.zooms_of(&m);
        if let Some(i) = zooms.iter().position(|z| z.start_ms <= s && s < z.end_ms) {
            self.pick_zoom(Some((m.id, i)), cx);
            return;
        }
        // up to the next zoom, the clip's end or the take's
        let next = zooms.iter().map(|z| z.start_ms).filter(|st| *st > s).min().unwrap_or(u64::MAX);
        let end = (s + NEW_LEN).min(next).min(c.out_ms).min(m.duration_ms);
        if end < s + SHORTEST {
            self.note = Some("No room for a zoom there.".into());
            cx.notify();
            return;
        }
        let z = Zoom { start_ms: s, end_ms: end, scale: lightair_render::zoom::DEFAULT_SCALE, focus: Focus::Pointer, auto: false };
        let mut at = 0;
        self.set_zooms(
            m.id,
            |zs| {
                zs.push(z);
                zs.sort_by_key(|z| z.start_ms);
                at = zs.iter().position(|x| *x == z).unwrap_or(0);
            },
            cx,
        );
        self.pick_zoom(Some((m.id, at)), cx);
    }

    /// The picked zoom changed (no longer automatic).
    fn change_zoom(&mut self, f: impl FnOnce(&mut Zoom), cx: &mut Context<Self>) {
        let Some((media, i)) = self.zoom_sel else { return };
        self.set_zooms(
            media,
            |zs| {
                if let Some(z) = zs.get_mut(i) {
                    f(z);
                    z.auto = false;
                }
            },
            cx,
        );
    }

    /// Deletes the picked zoom: true if there was one.
    pub(super) fn delete_zoom(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((media, i)) = self.zoom_sel.take() else { return false };
        self.set_zooms(media, |zs| {
            if i < zs.len() {
                zs.remove(i);
            }
        }, cx);
        true
    }

    /// A zoom block dragged to timeline time `t`.
    pub(super) fn drag_zoom(&mut self, media: Id, index: usize, clip: Id, part: ZoomPart, t: u64) {
        let Some(c) = self.seq().and_then(|s| s.clip(clip)).cloned() else { return };
        let Some(m) = self.seq().and_then(|s| s.media(media)).cloned() else { return };
        let zooms = self.zooms_of(&m);
        let Some(z) = zooms.get(index).copied() else { return };
        let s = in_source(&c, t);
        // its neighbours bound it
        let prev = zooms.iter().enumerate().filter(|(i, x)| *i != index && x.end_ms <= z.start_ms).map(|(_, x)| x.end_ms).max().unwrap_or(0);
        let next = zooms.iter().enumerate().filter(|(i, x)| *i != index && x.start_ms >= z.end_ms).map(|(_, x)| x.start_ms).min().unwrap_or(m.duration_ms.max(z.end_ms));
        let len = z.end_ms - z.start_ms;
        let (start, end) = match part {
            ZoomPart::Body { grab } => {
                let st = s.saturating_sub(grab).clamp(prev, next.saturating_sub(len).max(prev));
                (st, st + len)
            }
            ZoomPart::Start => (s.clamp(prev, z.end_ms.saturating_sub(SHORTEST)), z.end_ms),
            ZoomPart::End => (z.start_ms, s.clamp(z.start_ms + SHORTEST, next)),
        };
        if (start, end) == (z.start_ms, z.end_ms) {
            return;
        }
        // (a drag: one undo step when it ends)
        let mut zooms = zooms;
        if let Some(z) = zooms.get_mut(index) {
            z.start_ms = start;
            z.end_ms = end;
            z.auto = false;
        }
        if let Some(m) = self.project.as_mut().and_then(|p| p.sequence.media.iter_mut().find(|m| m.id == media)) {
            m.zooms = Some(zooms);
        }
        self.after_drag_change();
    }

    /// The zoom lane, under the tracks.
    pub(super) fn zoom_lane(&self, cx: &mut Context<Self>) -> Option<Div> {
        let takes = self.take_clips();
        if takes.is_empty() {
            return None;
        }
        let mut lane = div().id("ed-zoom-lane").relative().flex_1().h_full().overflow_hidden().cursor(CursorStyle::PointingHand);
        // where zooms can go: under the takes
        for (c, _) in &takes {
            let (l, w) = (self.x_of(c.start_ms), c.len_ms() as f32 / 1000.0 * self.pps());
            lane = lane.child(div().absolute().top(px(5.)).bottom(px(5.)).left(px(l)).w(px(w)).rounded(px(5.)).bg(rgba(0xFFFFFF08)));
        }
        let picked = self.zoom_sel;
        for b in self.blocks() {
            let (left, width) = (self.x_of(b.from), ((b.to - b.from) as f32 / 1000.0 * self.pps()).max(6.0));
            if left > 4000.0 || left + width < 0.0 {
                continue;
            }
            let on = picked == Some((b.media, b.index));
            let (media, index, clip, zoom) = (b.media, b.index, b.clip, b.zoom);
            let text = format!("{:.1}×{}", zoom.scale, if zoom.auto { " · Auto" } else { "" });
            let mut block = div()
                .id(SharedString::from(format!("ed-zoom-{media}-{index}-{clip}")))
                .absolute()
                .top(px(4.))
                .bottom(px(4.))
                .left(px(left))
                .w(px(width))
                .rounded(px(5.))
                .bg(rgb(0x7C3AED))
                .border_2()
                .border_color(if on { rgb(0xFFFFFF) } else { rgba(0x00000000) })
                .overflow_hidden()
                .flex()
                .items_center()
                .gap_1()
                .px_1p5()
                .text_size(px(11.))
                .text_color(rgb(0xFFFFFF))
                .cursor(CursorStyle::OpenHand)
                .child(ui::icon("zoom-in").size(px(11.)))
                .child(div().whitespace_nowrap().child(text))
                .when(matches!(zoom.focus, Focus::Pointer), |d| d.child(ui::icon("mouse-pointer-2").size(px(10.)).opacity(0.8)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.pick_zoom(Some((media, index)), cx);
                        let Some(t) = this.time_at(ev.position.x) else { return };
                        let Some(c) = this.seq().and_then(|s| s.clip(clip)).cloned() else { return };
                        let x = this.x_of(t);
                        let (l, r) = (this.x_of(on_timeline(&c, zoom.start_ms.max(c.in_ms))), this.x_of(on_timeline(&c, zoom.end_ms.min(c.out_ms))));
                        let part = if x - l < EDGE {
                            ZoomPart::Start
                        } else if r - x < EDGE {
                            ZoomPart::End
                        } else {
                            ZoomPart::Body { grab: in_source(&c, t).saturating_sub(zoom.start_ms) }
                        };
                        this.begin_drag(Drag::Zoom { media, index, clip, part });
                    }),
                )
                .on_mouse_down(MouseButton::Right, cx.listener(move |this, _, _, cx| this.pick_zoom(Some((media, index)), cx)))
                .context_menu({
                    let ed = cx.entity().downgrade();
                    move |m, _, _| {
                        let ed = ed.clone();
                        m.item(PopupMenuItem::new("Delete zoom").icon(ui::icon("trash")).on_click(move |_, _, cx| {
                            if let Some(e) = ed.upgrade() {
                                e.update(cx, |e, cx| {
                                    e.delete_zoom(cx);
                                });
                            }
                        }))
                    }
                });
            // its edges lengthen it
            for left_edge in [true, false] {
                block = block.child(div().absolute().top_0().bottom_0().when(left_edge, |d| d.left_0()).when(!left_edge, |d| d.right_0()).w(px(EDGE)).cursor(CursorStyle::ResizeLeftRight));
            }
            lane = lane.child(block);
        }
        let lane = lane.on_mouse_down(
            MouseButton::Left,
            cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                cx.stop_propagation();
                if let Some(t) = this.time_at(ev.position.x) {
                    this.add_zoom_at(t, cx);
                }
            }),
        );
        let header = div()
            .flex()
            .items_center()
            .gap_1()
            .w(px(HEADER_W))
            .flex_none()
            .h_full()
            .px_1()
            .border_r_1()
            .border_color(tok::border())
            .child(ui::icon("zoom-in").size(px(13.)).text_color(tok::text_tertiary()))
            .child(div().flex_1().text_size(px(11.)).text_color(tok::text_tertiary()).child("Zoom"))
            .child(ui::icon_button("ed-zoom-add", "plus", tok::text_secondary()).on_click(cx.listener(|this, _, _, cx| {
                let t = this.playhead;
                this.add_zoom_at(t, cx)
            })));
        Some(div().flex().h(px(ZOOM_H)).border_b_1().border_color(tok::border()).bg(rgba(0x7C3AED10)).child(header).child(lane))
    }

    /// The inspector for the picked zoom: its head and body.
    pub(super) fn zoom_inspector(&self, cx: &mut Context<Self>) -> Option<(AnyElement, Div)> {
        let (m, _, z) = self.picked_zoom()?;
        let head = self.inspector_head("zoom-in", "Zoom".into(), format!("{} · {:.1} s{}", m.name, (z.end_ms - z.start_ms) as f32 / 1000.0, if z.auto { " · Automatic" } else { "" }));
        let scale = (z.scale * 100.0).round() as i32;
        let pointer = matches!(z.focus, Focus::Pointer);
        let at = match z.focus {
            Focus::Point { x, y } => (x, y),
            Focus::Pointer => (0.5, 0.5),
        };
        // where it looks: a picture of the screen, a click on a spot looks there
        let (cols, rows) = (8usize, 5usize);
        let mut spots = div().relative().w_full().aspect_ratio(16.0 / 9.0).rounded(px(6.)).overflow_hidden().bg(tok::bg_canvas_well()).border_1().border_color(tok::border()).flex().flex_col();
        for r in 0..rows {
            let mut row = div().flex().flex_1();
            for k in 0..cols {
                let (fx, fy) = ((k as f32 + 0.5) / cols as f32, (r as f32 + 0.5) / rows as f32);
                row = row.child(div().id(("ed-zoom-spot", r * cols + k)).flex_1().cursor_pointer().hover(|d| d.bg(rgba(0x7C3AED40))).on_click(cx.listener(move |this, _, _, cx| {
                    this.change_zoom(|z| z.focus = Focus::Point { x: fx, y: fy }, cx)
                })));
            }
            spots = spots.child(row);
        }
        if !pointer {
            // what it shows, and its centre
            let (w, h) = (1.0 / z.scale.max(1.0), 1.0 / z.scale.max(1.0));
            let (x0, y0) = ((at.0 - w / 2.0).clamp(0.0, 1.0 - w), (at.1 - h / 2.0).clamp(0.0, 1.0 - h));
            spots = spots
                .child(div().absolute().left(relative(x0)).top(relative(y0)).w(relative(w)).h(relative(h)).border_1().border_color(rgb(0xA78BFA)).bg(rgba(0x7C3AED30)))
                .child(div().absolute().left(relative(at.0)).top(relative(at.1)).ml(px(-4.)).mt(px(-4.)).size(px(8.)).rounded_full().bg(rgb(0xFFFFFF)));
        }
        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .pb_4()
            .child(label("ZOOM"))
            .child(self.chips("ed-zoom-scale", &[("1.25×", 125), ("1.5×", 150), ("2×", 200), ("2.5×", 250), ("3×", 300), ("4×", 400)], scale, |this, v, cx| this.change_zoom(|z| z.scale = v as f32 / 100.0, cx), cx))
            .child(label("LOOKS AT"))
            .child(self.segmented("ed-zoom-focus".into(), &[("Follow pointer", true), ("A spot", false)], pointer, move |this, v, cx| {
                this.change_zoom(|z| z.focus = if v { Focus::Pointer } else { Focus::Point { x: at.0, y: at.1 } }, cx)
            }, cx))
            .when(!pointer, |d| d.child(spots).child(hint("Click where it should look.")))
            .when(pointer, |d| d.child(hint("The view follows the pointer while it's zoomed.")))
            .child(
                div().flex().pt_2().child(Button::new("ed-zoom-del").label("Delete zoom").icon(ui::icon("trash")).xsmall().outline().on_click(cx.listener(|this, _, _, cx| {
                    this.delete_zoom(cx);
                }))),
            )
            .child(hint("On the zoom track: drag a zoom to move it, its edges to lengthen it; click the track over a take to add one."));
        Some((head, body))
    }
}

impl Editor {
    /// For the check hook: a zoom added at 2 s on the first take, looking at a spot; then
    /// moved, lengthened, undone and deleted, each logged.
    pub fn hook_zooms(&mut self, cx: &mut Context<Self>) {
        let count = |this: &Self| this.take_clips().first().map_or(0, |(_, m)| this.zooms_of(m).len());
        let n0 = count(self);
        self.add_zoom_at(2_000, cx);
        let Some((m, i, z)) = self.picked_zoom() else {
            tracing::info!("CHECK zoom: none added");
            return;
        };
        tracing::info!(before = n0, after = count(self), start = z.start_ms, end = z.end_ms, lane = self.tracks_top() > RULER_H, "CHECK zoom added");
        self.change_zoom(|z| z.focus = Focus::Point { x: 0.3, y: 0.35 }, cx);
        let Some(clip) = self.take_clips().first().map(|(c, _)| c.id) else { return };
        // dragged: held 500 ms in, to 3.5 s; then its end to 5.6 s
        self.begin_drag(Drag::Zoom { media: m.id, index: i, clip, part: ZoomPart::Body { grab: 500 } });
        self.drag_zoom(m.id, i, clip, ZoomPart::Body { grab: 500 }, 3_500);
        self.drag_end(cx);
        let moved = self.picked_zoom().map(|(_, _, z)| (z.start_ms, z.end_ms));
        self.begin_drag(Drag::Zoom { media: m.id, index: i, clip, part: ZoomPart::End });
        self.drag_zoom(m.id, i, clip, ZoomPart::End, 5_600);
        self.drag_end(cx);
        let longer = self.picked_zoom().map(|(_, _, z)| (z.start_ms, z.end_ms));
        self.undo(true, cx);
        let undone = self.picked_zoom().map(|(_, _, z)| (z.start_ms, z.end_ms));
        tracing::info!(?moved, ?longer, ?undone, "CHECK zoom dragged");
        self.delete_selected(cx);
        tracing::info!(after_delete = count(self), picked = ?self.zoom_sel, "CHECK zoom deleted");
        self.undo(true, cx);
        self.zoom_sel = Some((m.id, i));
        self.seek(3_000, cx);
    }
}
