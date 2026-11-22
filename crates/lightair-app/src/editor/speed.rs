//! The Editor's speed of work (docs/plans/video-editor.md, phase 5): proxies for large videos,
//! made one at a time on a thread, and filmstrips on video clips.

use super::*;

impl Editor {
    /// The next proxy the project wants, made on a thread (one at a time).
    pub(super) fn make_proxies(&mut self, cx: &mut Context<Self>) {
        if self.proxying.is_some() {
            return;
        }
        let Some(m) = self.seq().and_then(|s| s.proxies_missing().into_iter().next()) else { return };
        if self.proxy_failed.contains(&m.path) {
            return;
        }
        self.proxying = Some((m.name.clone().into(), 0.0));
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let progress = tx.clone();
            let r = lightair_render::media::make_proxy(&m.path, &m.proxy_path(), m.duration_ms, |f| progress.send(Done::ProxyProgress(f)).is_ok()).map_err(|e| e.to_string());
            let _ = tx.send(Done::Proxied(m.path.clone(), r));
        });
        cx.notify();
    }

    pub(super) fn take_proxy(&mut self, file: PathBuf, r: Result<(), String>, cx: &mut Context<Self>) {
        let name = self.proxying.take().map(|p| p.0).unwrap_or_default();
        match r {
            Ok(()) => {
                // the player reads it from the next frame
                if let Some(p) = &self.project {
                    self.pending.lock().unwrap().seq = Some(p.sequence.clone());
                }
                if self.playing.is_none() {
                    self.request(self.playhead);
                }
            }
            Err(e) => {
                self.proxy_failed.push(file);
                self.note = Some(format!("A proxy for “{name}” couldn't be made: {e}").into());
            }
        }
        self.make_proxies(cx);
        cx.notify();
    }

    /// The project's proxies, for the inspector.
    pub(super) fn proxies_section(&self, body: Div, cx: &mut Context<Self>) -> Div {
        let Some(seq) = self.seq() else { return body };
        let wanting: Vec<&Media> = seq.media.iter().filter(|m| m.wants_proxy()).collect();
        let made = wanting.iter().filter(|m| m.proxy().is_some()).count();
        let status = match (&self.proxying, seq.proxies) {
            (_, false) => "Off: the player reads the original files.".to_string(),
            (Some((name, f)), _) => format!("Making a proxy for “{name}”… {:.0}%", f * 100.0),
            (None, _) if wanting.is_empty() => "On: videos larger than 1080p are edited through smaller copies (none here).".to_string(),
            (None, _) => format!("On: {made} of {} large videos edited through smaller copies. The export reads the originals.", wanting.len()),
        };
        body.child(label("PROXIES"))
            .child(self.chips("ed-proxies", &[("On", true), ("Off", false)], seq.proxies, |this, v, cx| {
                this.change(|s| s.proxies = v, cx);
                this.make_proxies(cx);
            }, cx))
            // (its state, shown as it was: hint() is an ⓘ now)
            .child(line(status))
    }
}

/// A video's thumbnails, for its clips on the timeline.
pub(super) struct Strip {
    pub every_ms: u64,
    pub aspect: f32,
    pub images: Vec<Arc<RenderImage>>,
}

/// Thumbnail height in pixels (drawn at about half that, on a Retina screen).
const STRIP_H: u32 = 80;

impl Editor {
    /// Filmstrips for the videos on video tracks, read on threads (once per file).
    pub(super) fn want_strips(&mut self) {
        let Some(seq) = self.seq() else { return };
        let mut wanted: Vec<Media> = Vec::new();
        for c in seq.tracks.iter().filter(|t| t.kind == TrackKind::Video).flat_map(|t| t.clips.iter()) {
            if let Some(m) = seq.media(c.media).filter(|m| m.kind == MediaKind::Video && !self.strips.contains_key(&m.path) && !wanted.iter().any(|w| w.path == m.path)) {
                wanted.push(m.clone());
            }
        }
        for m in wanted {
            self.strips.insert(m.path.clone(), None);
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                // about 400 along the whole video, at most two a second
                let every = (m.duration_ms / 400).max(500);
                let aspect = m.width.max(1) as f32 / m.height.max(1) as f32;
                // a proxy is quicker to read, when there is one
                let file = m.proxy().unwrap_or_else(|| m.path.clone());
                let strip = lightair_render::media::filmstrip(&file, m.duration_ms, every, STRIP_H, aspect).ok().map(|(frames, size)| {
                    let images = frames
                        .into_iter()
                        .filter_map(|f| image::RgbaImage::from_raw(size.0, size.1, f))
                        .map(|b| Arc::new(RenderImage::new([image::Frame::new(b)])))
                        .collect();
                    Arc::new(Strip { every_ms: every, aspect, images })
                });
                let _ = tx.send(Done::Strip(m.path, strip));
            });
        }
    }

    /// Thumbnails tiled along a video clip's visible part (`left`, `width`: its place in the
    /// lane; `height`: the clip's).
    pub(super) fn filmstrip(&self, c: &lightair_render::sequence::Clip, m: Option<&Media>, left: f32, width: f32, height: f32) -> Option<AnyElement> {
        let strip = self.strips.get(&m?.path)?.clone()?;
        let tw = (height * strip.aspect).max(8.0);
        let pps = self.pps();
        let first = ((-left).max(0.0) / tw).floor() * tw;
        let end = width.min(4000.0 - left);
        let mut tiles = div().absolute().top_0().left_0().w(px(width)).h(px(height)).overflow_hidden();
        let mut x = first;
        while x < end {
            let source = c.in_ms as f32 + x / pps * 1000.0 * c.speed;
            let i = ((source / strip.every_ms as f32) as usize).min(strip.images.len().saturating_sub(1));
            if let Some(image) = strip.images.get(i) {
                tiles = tiles.child(img(ImageSource::Render(image.clone())).absolute().left(px(x)).top_0().w(px(tw)).h(px(height)).object_fit(ObjectFit::Cover));
            }
            x += tw;
        }
        Some(tiles.into_any_element())
    }
}

impl Editor {
    /// For the screenshot hooks: a large video (next to the workspace) added, so a proxy is
    /// made; or (`play`) the project played, silent, for four seconds, counting the frames shown.
    pub fn hook_speed(&mut self, play: bool, cx: &mut Context<Self>) {
        if play {
            self.change(|s| s.tracks.iter_mut().for_each(|t| t.muted = true), cx);
            self.seek(0, cx);
            self.shown = 0;
            self.shown_since = None;
            self.play(cx);
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(std::time::Duration::from_secs(4)).await;
                let _ = this.update(cx, |this, cx| {
                    let span = this.shown_since.map_or(0.0, |s| s.elapsed().as_secs_f64());
                    let fps = this.shown.saturating_sub(1) as f64 / span.max(0.001);
                    tracing::info!(frames = this.shown, after_first_s = format!("{span:.2}"), fps = format!("{fps:.1}"), size = ?this.preview_box, "editor playback");
                    this.stop(cx);
                });
            })
            .detach();
            return;
        }
        let big = self.workspace.as_ref().and_then(|w| w.parent()).map(|d| d.join("ed-media").join("Big.mp4")).and_then(|p| Media::from_file(&p).ok());
        if let Some(m) = big {
            self.change(
                |s| {
                    let id = s.add_media(m);
                    s.append(id);
                },
                cx,
            );
        }
        self.pick_only(None);
        self.make_proxies(cx);
    }
}
