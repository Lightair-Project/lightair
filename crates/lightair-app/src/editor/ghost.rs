//! A ghost clip on the timeline while something is dragged over it: where it would land (the
//! track, a new one if none is free, the time with snapping) and how long it really is.

use super::*;

/// Where a drop would land.
#[derive(Clone, PartialEq)]
pub(super) struct Ghost {
    /// The track it goes on; `None`: a new one (above the video tracks, or below the audio).
    track: Option<usize>,
    kind: TrackKind,
    start: u64,
    len: u64,
    label: SharedString,
    /// The clip as it would be, and its medium (for its filmstrip or waveform); a Library item's
    /// thumbnail when its frames aren't read yet.
    clip: lightair_render::sequence::Clip,
    media: Option<Media>,
    thumb: Option<PathBuf>,
}

/// Extensions of sound-only and still files (a Library item's kind is guessed from its first).
const SOUND: [&str; 6] = ["wav", "m4a", "mp3", "flac", "aac", "aif"];
const STILL: [&str; 4] = ["png", "jpg", "jpeg", "gif"];

impl Editor {
    /// The ghost for `drag` with the pointer at `pos` (`None` off the tracks).
    fn ghost_for(&self, drag: &DraggedMedia, pos: Point<Pixels>) -> Option<Ghost> {
        let seq = self.seq()?;
        let t = self.time_at(pos.x).map(|t| self.snap(t as i64, None))?;
        let track = self.track_at(pos.y);
        // the drop, tried on a copy
        let mut s = seq.clone();
        let placed = match &drag.source {
            MediaSource::Media(id) => s.place(*id, t, track)?,
            MediaSource::Content(content, _) => s.place_content_on(content.clone(), t, 3_000, track),
            MediaSource::Item(item) => {
                let ext = item.files.first().and_then(|f| f.path.extension()).map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
                let kind = if SOUND.contains(&ext.as_str()) {
                    MediaKind::Audio
                } else if STILL.contains(&ext.as_str()) {
                    MediaKind::Image
                } else {
                    MediaKind::Video
                };
                let mut m = Media {
                    id: Id::new(),
                    name: item.title.clone(),
                    kind,
                    path: item.files.first().map(|f| f.path.clone()).unwrap_or_default(),
                    duration_ms: item.duration_ms.unwrap_or(5_000).max(1),
                    width: 1920,
                    height: 1080,
                    audio: Vec::new(),
                    take: None,
                    item: Some(item.id),
                    look: None,
                    transcript: None,
                };
                // a video's own sound keeps it on a picture track
                if kind == MediaKind::Video {
                    m.audio = vec![m.path.clone()];
                }
                let id = s.add_media(m);
                s.place(id, t, track)?
            }
        };
        let (ti, _) = s.find(placed)?;
        let c = s.clip(placed)?;
        let landed = &s.tracks[ti];
        // on a track the sequence already has, or a new one
        let existing = seq.tracks.iter().position(|tr| tr.id == landed.id);
        let media = s.media(c.media).cloned();
        let thumb = match &drag.source {
            MediaSource::Item(item) => item.thumbnail.clone().filter(|t| t.exists()),
            _ => media.as_ref().filter(|m| m.kind == MediaKind::Image).map(|m| m.path.clone()),
        };
        Some(Ghost { track: existing, kind: landed.kind, start: c.start_ms, len: c.len_ms(), label: drag.label.clone(), clip: c.clone(), media, thumb })
    }

    /// Follows a drag over the timeline.
    pub(super) fn drag_over(&mut self, drag: &DraggedMedia, pos: Point<Pixels>, inside: bool, cx: &mut Context<Self>) {
        let ghost = if inside { self.ghost_for(drag, pos) } else { None };
        // the card that follows the pointer gives way to the ghost
        self.drag_hidden.set(ghost.is_some());
        if ghost != self.ghost {
            self.ghost = ghost;
            cx.notify();
        }
    }

    /// The ghost, drawn over the timeline (positions from its top-left).
    pub(super) fn ghost_overlay(&self) -> Option<AnyElement> {
        let g = self.ghost.as_ref()?;
        let b = (*self.timeline_bounds.borrow())?;
        let seq = self.seq()?;
        let pps = self.pps();
        let left = HEADER_W + self.x_of(g.start);
        let width = (g.len as f32 / 1000.0 * pps).max(6.0);
        let rows: f32 = seq.tracks.iter().map(|t| if t.kind == TrackKind::Video { VIDEO_H } else { AUDIO_H }).sum();
        let (top, height, fresh) = match g.track {
            Some(ti) => {
                let above: f32 = seq.tracks.iter().take(ti).map(|t| if t.kind == TrackKind::Video { VIDEO_H } else { AUDIO_H }).sum();
                let h = if g.kind == TrackKind::Video { VIDEO_H } else { AUDIO_H };
                (RULER_H + above + 3.0, h - 6.0, false)
            }
            // a new video track goes on top (shown over the ruler), a new audio one below
            None if g.kind == TrackKind::Video => (2.0, RULER_H - 4.0, true),
            None => (RULER_H + rows + 3.0, AUDIO_H - 6.0, true),
        };
        let _ = b;
        // the clip's own pictures or sound in it
        let inside = match g.kind {
            TrackKind::Video if !fresh => self.filmstrip(&g.clip, g.media.as_ref(), left - HEADER_W, width, height).or_else(|| {
                let path = g.thumb.clone()?;
                let tw = height * 16.0 / 9.0;
                let mut row = div().absolute().top_0().left_0().w(px(width)).h(px(height)).overflow_hidden();
                let mut x = 0.0;
                while x < width.min(4000.0) {
                    row = row.child(img(path.clone()).absolute().left(px(x)).top_0().w(px(tw)).h(px(height)).object_fit(ObjectFit::Cover));
                    x += tw;
                }
                Some(row.into_any_element())
            }),
            TrackKind::Audio => self.wave(&g.clip, g.media.as_ref(), left - HEADER_W, width, height + 6.0),
            _ => None,
        };
        let colour = match g.kind {
            TrackKind::Video => rgba(0x0E7C8690),
            TrackKind::Audio => rgba(0x1E4E8C90),
        };
        Some(
            div()
                .absolute()
                .left(px(left))
                .top(px(top))
                .w(px(width))
                .h(px(height))
                .rounded(px(5.))
                .bg(colour)
                .border_2()
                .border_dashed()
                .border_color(rgba(0xFFFFFFC0))
                .overflow_hidden()
                .px_1p5()
                .flex()
                .items_center()
                .gap_2()
                .text_size(px(11.))
                .text_color(rgb(0xFFFFFF))
                .whitespace_nowrap()
                .children(inside.map(|e| div().absolute().top_0().left_0().size_full().opacity(0.55).child(e)))
                .child(
                    div()
                        .relative()
                        .flex()
                        .gap_2()
                        .px_1()
                        .rounded(px(3.))
                        .bg(rgba(0x00000090))
                        .child(g.label.clone())
                        .child(div().opacity(0.8).child(short(g.len)))
                        .when(fresh, |d| d.child(div().opacity(0.7).child("new track"))),
                )
                .into_any_element(),
        )
    }
}

impl Editor {
    /// For the screenshot hook: the workspace's newest Library item held over the main video
    /// track at 26 s (after the clips there), as a drag would, once the timeline is laid out.
    pub fn hook_ghost(&mut self, cx: &mut Context<Self>) {
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(1500)).await;
            let _ = this.update(cx, |this, cx| {
                let (Some(item), Some(b)) = (this.library.first().cloned(), *this.timeline_bounds.borrow()) else { return };
                let Some(main) = this.seq().and_then(|s| s.tracks.iter().rposition(|t| t.kind == TrackKind::Video)) else { return };
                let y = this.track_top(main).unwrap_or(0.0) + VIDEO_H / 2.0;
                let x = f32::from(b.origin.x) + HEADER_W + this.x_of(26_000);
                let drag = DraggedMedia::new(item.title.clone(), MediaSource::Item(item), None, "film", &this.drag_hidden);
                this.drag_over(&drag, point(px(x), px(y)), true, cx);
            });
        })
        .detach();
    }
}
