//! The zoom track, as Screen Studio has it: a lane above the tracks with the project's zooms as
//! blocks where they play. A zoom magnifies the whole canvas (every layer, as the frame is
//! composed; lightair-render's `Sequence::zooms`, timeline time). A press on the lane over a
//! video, picture or take adds one there; a block is picked with a click, moved by dragging it and
//! lengthened by its edges. A zoom either follows the pointer (of a take under it: it needs the
//! take's input log) or shows a region: with a region zoom picked, the player shows the whole
//! frame with the region on it, to drag (inside it to move it, a corner to size it, elsewhere to
//! draw a new one; `stage.rs`). The inspector sets how far it zooms and which of the two it is.
//! A take's own zooms (its look's, the automatic ones) are moved onto the track when it's in a
//! project (`Sequence::absorb_take_zooms`), so the track is where every zoom is.

use super::*;
use lightair_render::edit::{Focus, Zoom};

pub(super) const ZOOM_H: f32 = 34.0;
/// A zoom at least this long (ms).
const SHORTEST: u64 = 300;
/// A new zoom's length (ms).
const NEW_LEN: u64 = 2_000;

/// Which part of a zoom block is dragged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ZoomPart {
    /// The whole block, held `grab` ms after its start.
    Body {
        grab: u64,
    },
    Start,
    End,
}

impl Editor {
    /// Where a medium's clips are on the picture tracks (timeline ms).
    pub(super) fn spans_of(&self, media: Id) -> Vec<(u64, u64)> {
        let Some(s) = self.seq() else { return Vec::new() };
        s.tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Video)
            .flat_map(|t| &t.clips)
            .filter(|c| c.media == media && c.content.is_none())
            .map(|c| (c.start_ms, c.end_ms()))
            .collect()
    }

    /// How many zooms play over a medium's clips (a take's inspector says).
    pub(super) fn zooms_over(&self, media: Id) -> usize {
        let spans = self.spans_of(media);
        self.zooms().iter().filter(|z| spans.iter().any(|(a, b)| z.start_ms < *b && *a < z.end_ms)).count()
    }

    /// Where the tracks start, from the timeline's top: under the ruler and the zoom track.
    pub(super) fn tracks_top(&self) -> f32 {
        RULER_H + if self.has_zoomable() { ZOOM_H } else { 0.0 }
    }

    /// A video, picture or take on a picture track: there's something to zoom.
    fn has_zoomable(&self) -> bool {
        !self.zoomable_spans().is_empty()
    }

    /// The pictured clips' spans on the timeline (where zooms can go).
    fn zoomable_spans(&self) -> Vec<(u64, u64)> {
        let Some(s) = self.seq() else { return Vec::new() };
        s.tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Video && !t.hidden)
            .flat_map(|t| &t.clips)
            .filter(|c| {
                c.content.is_none()
                    && s.media(c.media).is_some_and(|m| m.studio_take.is_some() || m.kind != MediaKind::Audio)
            })
            .map(|c| (c.start_ms, c.end_ms()))
            .collect()
    }

    /// Whether a take is under timeline time `t` (a zoom there can follow its pointer).
    fn take_under(&self, t: u64) -> bool {
        self.seq().is_some_and(|s| {
            s.tracks.iter().filter(|tr| tr.kind == TrackKind::Video && !tr.hidden).flat_map(|tr| &tr.clips).any(|c| {
                c.content.is_none()
                    && c.start_ms <= t
                    && t < c.end_ms()
                    && s.media(c.media).is_some_and(|m| m.studio_take.is_some())
            })
        })
    }

    fn zooms(&self) -> Vec<Zoom> {
        self.seq().map(|s| s.zooms.clone()).unwrap_or_default()
    }

    pub(super) fn picked_zoom(&self) -> Option<(usize, Zoom)> {
        let (_, i) = self.zoom_sel?;
        Some((i, *self.seq()?.zooms.get(i)?))
    }

    fn pick_zoom(&mut self, i: Option<usize>, cx: &mut Context<Self>) {
        if i.is_some() {
            self.pick_only(None);
        }
        self.zoom_sel = i.map(|i| (Id::nil(), i));
        // to the zoom, so what it shows (and its region) is in the player
        if let Some(z) = i.and_then(|i| self.zooms().get(i).copied())
            && (self.playhead < z.start_ms || self.playhead >= z.end_ms)
        {
            self.seek(z.start_ms + (z.end_ms - z.start_ms) / 2, cx);
        }
        cx.notify();
    }

    /// The picked zoom when it shows a region (set on the player): its place and the zoom.
    pub(super) fn region_zoom(&self) -> Option<(usize, Zoom)> {
        let (i, z) = self.picked_zoom()?;
        if matches!(z.focus, Focus::Pointer) && self.take_under((z.start_ms + z.end_ms) / 2) {
            return None;
        }
        Some((i, z))
    }

    /// The player with or without the zooms: without while a zoom's region is being set.
    pub(super) fn sync_zoom_view(&mut self) {
        let hide = self.region_zoom().is_some();
        let changed = {
            let mut p = self.pending.lock().unwrap();
            let changed = p.zooms_hidden != hide;
            p.zooms_hidden = hide;
            if changed && let Some(pr) = &self.project {
                p.seq = Some(pr.sequence.clone());
            }
            changed
        };
        if changed {
            self.request(self.playhead);
        }
    }

    /// The box a zoom's region is measured in (result pixels): the whole frame.
    pub(super) fn zoom_box(&self) -> Option<(f32, f32, f32, f32)> {
        let s = self.seq()?;
        Some((0.0, 0.0, s.size.0 as f32, s.size.1 as f32))
    }

    /// A zoom added at timeline time `t` (over a video, picture or take there), picked.
    pub(super) fn add_zoom_at(&mut self, t: u64, cx: &mut Context<Self>) {
        if !self.zoomable_spans().iter().any(|(a, b)| *a <= t && t < *b) {
            self.note = Some("Zooms go over videos, pictures and takes: put the playhead over one.".into());
            cx.notify();
            return;
        }
        let zooms = self.zooms();
        if let Some(i) = zooms.iter().position(|z| z.start_ms <= t && t < z.end_ms) {
            self.pick_zoom(Some(i), cx);
            return;
        }
        // up to the next zoom and the end
        let total = self.seq().map_or(0, |s| s.duration_ms());
        let next = zooms.iter().map(|z| z.start_ms).filter(|st| *st > t).min().unwrap_or(u64::MAX);
        let end = (t + NEW_LEN).min(next).min(total);
        if end < t + SHORTEST {
            self.note = Some("No room for a zoom there.".into());
            cx.notify();
            return;
        }
        // over a take it follows the take's pointer; elsewhere a region, at first the middle
        let focus = if self.take_under(t) { Focus::Pointer } else { Focus::Point { x: 0.5, y: 0.5 } };
        let z = Zoom { start_ms: t, end_ms: end, scale: lightair_render::zoom::DEFAULT_SCALE, focus, auto: false };
        self.change(
            |s| {
                s.zooms.push(z);
                s.zooms.sort_by_key(|z| z.start_ms);
            },
            cx,
        );
        let at = self.zooms().iter().position(|x| *x == z);
        self.pick_zoom(at, cx);
    }

    /// The picked zoom changed (no longer automatic).
    pub(super) fn change_zoom(&mut self, f: impl FnOnce(&mut Zoom), cx: &mut Context<Self>) {
        let Some((_, i)) = self.zoom_sel else { return };
        self.change(
            |s| {
                if let Some(z) = s.zooms.get_mut(i) {
                    f(z);
                    z.auto = false;
                }
            },
            cx,
        );
    }

    /// Deletes the picked zoom: true if there was one.
    pub(super) fn delete_zoom(&mut self, cx: &mut Context<Self>) -> bool {
        let Some((_, i)) = self.zoom_sel.take() else { return false };
        self.change(
            |s| {
                if i < s.zooms.len() {
                    s.zooms.remove(i);
                }
            },
            cx,
        );
        true
    }

    /// A zoom block dragged to timeline time `t` (`media` and `clip` unused: zooms are the
    /// project's).
    pub(super) fn drag_zoom(&mut self, _media: Id, index: usize, _clip: Id, part: ZoomPart, t: u64) {
        let zooms = self.zooms();
        let Some(z) = zooms.get(index).copied() else { return };
        let total = self.seq().map_or(0, |s| s.duration_ms());
        // its neighbours bound it, and the project's end
        let prev = zooms
            .iter()
            .enumerate()
            .filter(|(i, x)| *i != index && x.end_ms <= z.start_ms)
            .map(|(_, x)| x.end_ms)
            .max()
            .unwrap_or(0);
        let next = zooms
            .iter()
            .enumerate()
            .filter(|(i, x)| *i != index && x.start_ms >= z.end_ms)
            .map(|(_, x)| x.start_ms)
            .min()
            .unwrap_or(total.max(z.end_ms));
        let len = z.end_ms - z.start_ms;
        let (start, end) = match part {
            ZoomPart::Body { grab } => {
                let st = t.saturating_sub(grab).clamp(prev, next.saturating_sub(len).max(prev));
                (st, st + len)
            }
            ZoomPart::Start => (t.clamp(prev, z.end_ms.saturating_sub(SHORTEST)), z.end_ms),
            ZoomPart::End => (z.start_ms, t.clamp(z.start_ms + SHORTEST, next)),
        };
        if (start, end) == (z.start_ms, z.end_ms) {
            return;
        }
        // (a drag: one undo step when it ends)
        if let Some(p) = self.project.as_mut()
            && let Some(z) = p.sequence.zooms.get_mut(index)
        {
            z.start_ms = start;
            z.end_ms = end;
            z.auto = false;
        }
        self.after_drag_change();
    }

    /// The zoom lane, above the tracks.
    pub(super) fn zoom_lane(&self, cx: &mut Context<Self>) -> Option<Div> {
        if !self.has_zoomable() {
            return None;
        }
        let mut lane =
            div().id("ed-zoom-lane").relative().flex_1().h_full().overflow_hidden().cursor(CursorStyle::PointingHand);
        // where zooms can go: over the videos, pictures and takes
        for (a, b) in self.zoomable_spans() {
            let (l, w) = (self.x_of(a), (b - a) as f32 / 1000.0 * self.pps());
            lane = lane.child(
                div().absolute().top(px(5.)).bottom(px(5.)).left(px(l)).w(px(w)).rounded(px(5.)).bg(rgba(0xFFFFFF08)),
            );
        }
        let picked = self.zoom_sel.map(|(_, i)| i);
        for (index, zoom) in self.zooms().into_iter().enumerate() {
            let (left, width) =
                (self.x_of(zoom.start_ms), ((zoom.end_ms - zoom.start_ms) as f32 / 1000.0 * self.pps()).max(6.0));
            if left > 4000.0 || left + width < 0.0 {
                continue;
            }
            let on = picked == Some(index);
            let pointer = matches!(zoom.focus, Focus::Pointer) && self.take_under((zoom.start_ms + zoom.end_ms) / 2);
            let text = format!("{:.1}×{}", zoom.scale, if zoom.auto { " · Auto" } else { "" });
            let mut block = div()
                .id(SharedString::from(format!("ed-zoom-{index}-{}", zoom.start_ms)))
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
                .child(ui::icon(if pointer { "mouse-pointer-2" } else { "scan" }).size(px(10.)).opacity(0.8))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, ev: &MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.pick_zoom(Some(index), cx);
                        let Some(t) = this.time_at(ev.position.x) else { return };
                        let x = this.x_of(t);
                        let (l, r) = (this.x_of(zoom.start_ms), this.x_of(zoom.end_ms));
                        let part = if x - l < EDGE {
                            ZoomPart::Start
                        } else if r - x < EDGE {
                            ZoomPart::End
                        } else {
                            ZoomPart::Body { grab: t.saturating_sub(zoom.start_ms) }
                        };
                        this.begin_drag(Drag::Zoom { media: Id::nil(), index, clip: Id::nil(), part });
                    }),
                )
                .on_mouse_down(MouseButton::Right, cx.listener(move |this, _, _, cx| this.pick_zoom(Some(index), cx)))
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
                block = block.child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .when(left_edge, |d| d.left_0())
                        .when(!left_edge, |d| d.right_0())
                        .w(px(EDGE))
                        .cursor(CursorStyle::ResizeLeftRight),
                );
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
        let hidden = self.seq().is_some_and(|s| s.zooms_hidden);
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
            // the zoom track switched off (its zooms kept, not drawn) or on
            .child(
                ui::icon_button("ed-zoom-hide", if hidden { "eye-off" } else { "eye" }, tok::text_secondary())
                    .on_click(cx.listener(|this, _, _, cx| this.change(|s| s.zooms_hidden = !s.zooms_hidden, cx))),
            )
            .child(ui::icon_button("ed-zoom-add", "plus", tok::text_secondary()).on_click(cx.listener(
                |this, _, _, cx| {
                    let t = this.playhead;
                    this.add_zoom_at(t, cx)
                },
            )));
        Some(
            div()
                .flex()
                .h(px(ZOOM_H))
                .border_b_1()
                .border_color(tok::border())
                .bg(rgba(0x7C3AED10))
                .child(header)
                // switched off: kept, faded
                .child(lane.when(hidden, |l| l.opacity(0.4))),
        )
    }

    /// The inspector for the picked zoom: its head and body.
    pub(super) fn zoom_inspector(&self, cx: &mut Context<Self>) -> Option<(AnyElement, Div)> {
        let (_, z) = self.picked_zoom()?;
        let head = self.inspector_head(
            "zoom-in",
            "Zoom".into(),
            format!(
                "The whole frame · {:.1} s{}",
                (z.end_ms - z.start_ms) as f32 / 1000.0,
                if z.auto { " · Automatic" } else { "" }
            ),
        );
        let scale = (z.scale * 100.0).round() as i32;
        let take = self.take_under((z.start_ms + z.end_ms) / 2);
        let pointer = take && matches!(z.focus, Focus::Pointer);
        let at = match z.focus {
            Focus::Point { x, y } => (x, y),
            Focus::Pointer => (0.5, 0.5),
        };
        // where it looks: a picture of the frame, a click on a spot looks there
        let (cols, rows) = (8usize, 5usize);
        let mut spots = div()
            .relative()
            .w_full()
            .aspect_ratio(16.0 / 9.0)
            .rounded(px(6.))
            .overflow_hidden()
            .bg(tok::bg_canvas_well())
            .border_1()
            .border_color(tok::border())
            .flex()
            .flex_col();
        for r in 0..rows {
            let mut row = div().flex().flex_1();
            for k in 0..cols {
                let (fx, fy) = ((k as f32 + 0.5) / cols as f32, (r as f32 + 0.5) / rows as f32);
                row = row.child(
                    div()
                        .id(("ed-zoom-spot", r * cols + k))
                        .flex_1()
                        .cursor_pointer()
                        .hover(|d| d.bg(rgba(0x7C3AED40)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.change_zoom(|z| z.focus = Focus::Point { x: fx, y: fy }, cx)
                        })),
                );
            }
            spots = spots.child(row);
        }
        if !pointer {
            // what it shows, and its centre
            let (w, h) = (1.0 / z.scale.max(1.0), 1.0 / z.scale.max(1.0));
            let (x0, y0) = ((at.0 - w / 2.0).clamp(0.0, 1.0 - w), (at.1 - h / 2.0).clamp(0.0, 1.0 - h));
            spots = spots
                .child(
                    div()
                        .absolute()
                        .left(relative(x0))
                        .top(relative(y0))
                        .w(relative(w))
                        .h(relative(h))
                        .border_1()
                        .border_color(rgb(0xA78BFA))
                        .bg(rgba(0x7C3AED30)),
                )
                .child(
                    div()
                        .absolute()
                        .left(relative(at.0))
                        .top(relative(at.1))
                        .ml(px(-4.))
                        .mt(px(-4.))
                        .size(px(8.))
                        .rounded_full()
                        .bg(rgb(0xFFFFFF)),
                );
        }
        let body = div()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .pb_4()
            .child(label("ZOOM"))
            .child(self.chips("ed-zoom-scale", &[("1.25×", 125), ("1.5×", 150), ("2×", 200), ("2.5×", 250), ("3×", 300), ("4×", 400)], scale, |this, v, cx| this.change_zoom(|z| z.scale = v as f32 / 100.0, cx), cx))
            .child(label("SHOWS"))
            .when(take, |d| {
                d.child(self.segmented("ed-zoom-focus".into(), &[("Follow pointer", true), ("A region", false)], pointer, move |this, v, cx| {
                    this.change_zoom(|z| z.focus = if v { Focus::Pointer } else { Focus::Point { x: at.0, y: at.1 } }, cx)
                }, cx))
            })
            .when(!pointer, |d| d.child(spots).child(hint("A region of the whole frame: drag on the player (inside it to move it, a corner to size it, elsewhere to draw a new one), or click here where it should look.")))
            .when(pointer, |d| d.child(hint("The view follows the take's pointer while it's zoomed.")))
            .child(
                div().flex().pt_2().child(Button::new("ed-zoom-del").label("Delete zoom").icon(ui::icon("trash")).xsmall().outline().on_click(cx.listener(|this, _, _, cx| {
                    this.delete_zoom(cx);
                }))),
            )
            .child(hint("On the zoom track: drag a zoom to move it, its edges to lengthen it; click the track over a video, picture or take to add one. A zoom magnifies the whole frame."));
        Some((head, body))
    }
}

impl Editor {
    /// For the check hook: a zoom added at 2 s, made a region; then moved, lengthened, undone
    /// and deleted, each logged.
    pub fn hook_zooms(&mut self, cx: &mut Context<Self>) {
        let count = |this: &Self| this.zooms().len();
        let n0 = count(self);
        self.add_zoom_at(2_000, cx);
        let Some((i, z)) = self.picked_zoom() else {
            tracing::info!("CHECK zoom: none added");
            return;
        };
        tracing::info!(
            before = n0,
            after = count(self),
            start = z.start_ms,
            end = z.end_ms,
            lane = self.tracks_top() > RULER_H,
            "CHECK zoom added"
        );
        self.change_zoom(|z| z.focus = Focus::Point { x: 0.3, y: 0.35 }, cx);
        // a region now: the player shows the whole frame to set it on
        self.sync_zoom_view();
        tracing::info!(
            region = self.region_zoom().is_some(),
            zooms_hidden = self.pending.lock().unwrap().zooms_hidden,
            "CHECK zoom region"
        );
        // dragged: held 500 ms in, to 3.5 s; then its end to 5.6 s
        self.begin_drag(Drag::Zoom { media: Id::nil(), index: i, clip: Id::nil(), part: ZoomPart::Body { grab: 500 } });
        self.drag_zoom(Id::nil(), i, Id::nil(), ZoomPart::Body { grab: 500 }, 3_500);
        self.drag_end(cx);
        let moved = self.picked_zoom().map(|(_, z)| (z.start_ms, z.end_ms));
        self.begin_drag(Drag::Zoom { media: Id::nil(), index: i, clip: Id::nil(), part: ZoomPart::End });
        self.drag_zoom(Id::nil(), i, Id::nil(), ZoomPart::End, 5_600);
        self.drag_end(cx);
        let longer = self.picked_zoom().map(|(_, z)| (z.start_ms, z.end_ms));
        self.undo(true, cx);
        let undone = self.picked_zoom().map(|(_, z)| (z.start_ms, z.end_ms));
        tracing::info!(?moved, ?longer, ?undone, "CHECK zoom dragged");
        self.delete_selected(cx);
        tracing::info!(after_delete = count(self), picked = ?self.zoom_sel, "CHECK zoom deleted");
        self.undo(true, cx);
        self.zoom_sel = Some((Id::nil(), i));
        self.seek(3_000, cx);
    }
}
