//! Sound in the Editor (docs/plans/video-editor.md, phase 4): waveforms on clips, the Audio
//! panel (music and sounds), the inspector's volume, volume keys, fades, detaching a video's
//! sound and cleaning up voices (made on a thread, then mixed in their place).

use super::*;
use gpui_kit::component::Disableable;
use lightair_render::sequence::{Clip, VoiceClean, VolumeKey};

/// The file a medium's sound is drawn from.
pub(super) fn sound_file(m: &Media) -> Option<PathBuf> {
    match m.kind {
        MediaKind::Image => None,
        MediaKind::Audio => Some(m.path.clone()),
        MediaKind::Video => m.audio.first().cloned(),
    }
}

/// Voice clean-up strengths offered: (name, amount; negative: off).
const VOICE: [(&str, i32); 4] = [("Off", -1), ("Light", 25), ("Medium", 50), ("Strong", 85)];

impl Editor {
    /// Waveforms for the clips on audio tracks, read on threads (once per file).
    pub(super) fn want_peaks(&mut self) {
        let Some(seq) = self.seq() else { return };
        let files: Vec<PathBuf> = seq
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Audio)
            .flat_map(|t| t.clips.iter())
            .filter_map(|c| seq.media(c.media).and_then(sound_file))
            .filter(|f| !self.peaks.contains_key(f))
            .collect();
        for f in files {
            self.peaks.insert(f.clone(), None);
            let tx = self.tx.clone();
            std::thread::spawn(move || {
                let p = lightair_render::sound::peaks(&f).ok().map(Arc::new);
                let _ = tx.send(Done::Peaks(f, p));
            });
        }
    }

    /// A clip's waveform, shaped by its volume and fades, for the visible part of the clip
    /// (`left`, `width`: its place in the lane).
    pub(super) fn wave(&self, c: &Clip, m: Option<&Media>, left: f32, width: f32, height: f32) -> Option<AnyElement> {
        let peaks = self.peaks.get(&sound_file(m?)?)?.clone()?;
        let pps = self.pps();
        let (from, to) = ((-left).max(0.0), width.min(4000.0 - left));
        let mut bars: Vec<(f32, f32)> = Vec::new();
        let per = lightair_render::sound::PEAKS_PER_SECOND as f32;
        let mut x = (from / 2.0).floor() * 2.0;
        while x < to {
            let t0 = c.start_ms as f32 + x / pps * 1000.0;
            let t1 = t0 + 2.0 / pps * 1000.0;
            let s = |t: f32| c.in_ms as f32 + (t - c.start_ms as f32) * c.speed;
            let a = (s(t0) / 1000.0 * per) as usize;
            let b = ((s(t1) / 1000.0 * per) as usize).max(a + 1);
            let peak = peaks.get(a..b.min(peaks.len())).map_or(0.0, |p| p.iter().fold(0f32, |m, v| m.max(*v)));
            // drawn in decibels (48 dB tall), as editors do: quiet sound still shows
            let level = peak * c.gain_at(t0 as u64);
            let db = if level > 0.0 { 20.0 * level.log10() } else { -96.0 };
            bars.push((x, ((db + 48.0) / 48.0).clamp(0.0, 1.0)));
            x += 2.0;
        }
        let colour = if c.muted { rgba(0xFFFFFF40) } else { rgba(0xBFDBFEE0) };
        Some(
            canvas(
                move |_, _, _| {},
                move |b, _, window, _| {
                    let mid = b.origin.y + b.size.height / 2.0;
                    let full = f32::from(b.size.height) * 0.9;
                    for (x, v) in &bars {
                        let h = (full * v).max(1.0);
                        window.paint_quad(fill(Bounds::new(point(b.origin.x + px(*x), mid - px(h / 2.0)), size(px(1.5), px(h))), colour));
                    }
                },
            )
            .absolute()
            .left_0()
            .top(px(height * 0.3))
            .w(px(width))
            .h(px(height * 0.62))
            .into_any_element(),
        )
    }

    /// Voices asked for and not cleaned yet: cleaned on a thread.
    pub(super) fn clean_voices(&mut self, cx: &mut Context<Self>) {
        if self.cleaning {
            return;
        }
        let Some(seq) = self.seq().cloned() else { return };
        if lightair_render::sound::voices_to_clean(&seq).is_empty() {
            return;
        }
        self.cleaning = true;
        self.note = Some("Cleaning up the voice…".into());
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let r = lightair_render::sound::prepare(&seq).map_err(|e| e.to_string());
            let _ = tx.send(Done::Cleaned(r));
        });
        cx.notify();
    }

    pub(super) fn take_cleaned(&mut self, r: Result<(), String>, cx: &mut Context<Self>) {
        self.cleaning = false;
        self.note = Some(match r {
            Ok(()) => "The voice is cleaned up.".into(),
            Err(e) => format!("The voice couldn't be cleaned up: {e}").into(),
        });
        // asked for more meanwhile?
        self.clean_voices(cx);
        cx.notify();
    }

    // ------------------------------------------------------------ the media panel's Audio tab

    pub(super) fn audio_tab(&self, cx: &mut Context<Self>) -> Div {
        let sounds: Vec<Media> = self.seq().map(|s| s.media.iter().filter(|m| m.kind == MediaKind::Audio).cloned().collect()).unwrap_or_default();
        let mut list = div().flex().flex_col().gap_1();
        for (n, m) in sounds.iter().enumerate() {
            let (id, name) = (m.id, m.name.clone());
            let drag = DraggedMedia::new(name.clone(), MediaSource::Media(id), None, "music", &self.drag_hidden);
            list = list.child(
                div()
                    .id(("ed-snd", n))
                    .flex()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1p5()
                    .rounded(px(6.))
                    .bg(tok::bg_app())
                    .cursor_pointer()
                    .hover(|d| d.bg(tok::bg_selected()))
                    .child(ui::icon("music").size(px(14.)).text_color(tok::text_secondary()))
                    .child(div().flex_1().min_w_0().text_size(px(12.)).overflow_hidden().text_ellipsis().whitespace_nowrap().child(name))
                    .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(short(m.duration_ms)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        let at = this.playhead;
                        this.add(MediaSource::Media(id), Some(at), None, cx)
                    }))
                    .on_drag(drag, |d, _, _, cx| cx.new(|_| d.clone())),
            );
        }
        div()
            .flex()
            .flex_col()
            .gap_2()
            .child(
                div()
                    .flex()
                    .items_center()
                    .child(div().flex_1().child(heading("MUSIC AND SOUNDS")))
                    .child(Button::new("ed-import-snd").label("Import").icon(ui::icon("plus")).xsmall().outline().on_click(cx.listener(|this, _, w, cx| this.import(w, cx)))),
            )
            .child(if sounds.is_empty() { hint("Import music, voice-overs or sound effects: they're added at the playhead on an audio track.") } else { list })
    }

    // ------------------------------------------------------------ the inspector

    pub(super) fn audio_section(&self, c: &Clip, m: &Media, body: Div, cx: &mut Context<Self>) -> Div {
        let cid = c.id;
        let t = self.playhead;
        let on_video = self.seq().and_then(|s| s.find(cid)).is_some_and(|(ti, _)| self.seq().is_some_and(|s| s.tracks[ti].kind == TrackKind::Video));
        let detachable = on_video && !m.audio.is_empty() && !c.muted;
        let inside = c.start_ms <= t && t < c.end_ms();
        let rel = t.saturating_sub(c.start_ms);
        let here = c.volume_keys.iter().any(|k| k.at_ms.abs_diff(rel) < 40);
        let prev = c.volume_keys.iter().rev().find(|k| k.at_ms + 40 < rel).map(|k| c.start_ms + k.at_ms);
        let next = c.volume_keys.iter().find(|k| k.at_ms > rel + 40).map(|k| c.start_ms + k.at_ms);
        let keys = div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                ui::icon_button("ed-vprev", "chevron-left", if prev.is_some() { tok::text_secondary() } else { tok::text_tertiary() })
                    .on_click(cx.listener(move |this, _, _, cx| if let Some(p) = prev { this.seek(p, cx) })),
            )
            .child(
                Button::new("ed-vkey")
                    .label(if here { "Remove volume key" } else { "Add volume key" })
                    .icon(ui::icon("diamond"))
                    .xsmall()
                    .outline()
                    .disabled(!inside)
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_volume_key(cid, cx))),
            )
            .child(
                ui::icon_button("ed-vnext", "chevron-right", if next.is_some() { tok::text_secondary() } else { tok::text_tertiary() })
                    .on_click(cx.listener(move |this, _, _, cx| if let Some(n) = next { this.seek(n, cx) })),
            );
        let voice = c.voice.map_or(-1, |v| (v.amount * 100.0).round() as i32);
        let podcast = c.voice.is_some_and(|v| v.podcast);
        let shown = c.volume_at(t.clamp(c.start_ms, c.end_ms().saturating_sub(1)));
        body.child(label("VOLUME"))
            .child(self.stepper("ed-vol", "Volume", format!("{:.0}%", shown * 100.0), move |this, d, cx| {
                let at = this.playhead;
                this.change(
                    |s| {
                        if let Some(c) = s.clip_mut(cid) {
                            let v = (c.volume_at(at) + d * 0.1).clamp(0.0, 4.0);
                            c.set_volume_at(at, (v * 10.0).round() / 10.0);
                        }
                    },
                    cx,
                )
            }, cx))
            .child(self.chips("ed-mute", &[("Sound on", false), ("Muted", true)], c.muted, move |this, v, cx| {
                this.change(|s| if let Some(c) = s.clip_mut(cid) { c.muted = v }, cx)
            }, cx))
            .child(keys)
            .child(hint(match (c.volume_keys.len(), inside) {
                (_, false) => "Move the playhead over the clip to set its volume keys.".to_string(),
                (0, _) => "Volume keys make the sound rise and fall: add one, move, change the volume.".to_string(),
                (n, _) => format!("{n} volume keys: the volume above sets the one at the playhead."),
            }))
            .map(|b| self.fades_section(c, b, cx))
            .child(label("UNDER VOICES"))
            .child(self.chips("ed-duck", &[("As it is", false), ("Turned down", true)], c.duck, move |this, v, cx| {
                this.change(|s| if let Some(c) = s.clip_mut(cid) { c.duck = v }, cx)
            }, cx))
            .child(hint("Music: turned down while the other clips' sound plays, back up after."))
            .child(label("CLEAN UP VOICE"))
            .child(self.chips("ed-voice", &VOICE, voice, move |this, v, cx| {
                this.change(
                    |s| {
                        if let Some(c) = s.clip_mut(cid) {
                            c.voice = (v >= 0).then(|| VoiceClean { amount: v as f32 / 100.0, podcast: c.voice.is_some_and(|w| w.podcast) });
                        }
                    },
                    cx,
                )
            }, cx))
            .when(voice >= 0, |d| {
                d.child(self.chips("ed-podcast", &[("Natural", false), ("Podcast", true)], podcast, move |this, v, cx| {
                    this.change(
                        |s| {
                            if let Some(w) = s.clip_mut(cid).and_then(|c| c.voice.as_mut()) {
                                w.podcast = v;
                            }
                        },
                        cx,
                    )
                }, cx))
            })
            .child(hint("Noise, a gate, harsh s-sounds, tone and level, as the live mixer's Voice Clarity; made once on this computer."))
            .when(detachable, |d| {
                d.child(label("SOUND")).child(div().flex().child(
                    Button::new("ed-detach").label("Detach sound").icon(ui::icon("unlink")).xsmall().outline().on_click(cx.listener(move |this, _, _, cx| {
                        let mut made = None;
                        this.change(|s| made = s.detach_sound(cid), cx);
                        if made.is_some() {
                            this.pick_only(made);
                            this.note = Some("The sound is on an audio track of its own.".into());
                        }
                        cx.notify();
                    })),
                ))
            })
    }

    /// Fade in and out: the picture and the sound together.
    pub(super) fn fades_section(&self, c: &Clip, body: Div, cx: &mut Context<Self>) -> Div {
        let (cid, len) = (c.id, c.len_ms());
        let fade = move |this: &mut Self, out: bool, d: f32, cx: &mut Context<Self>| {
            this.change(
                |s| {
                    if let Some(c) = s.clip_mut(cid) {
                        let f = if out { &mut c.fade_out_ms } else { &mut c.fade_in_ms };
                        *f = ((*f as f32 + d * 250.0).max(0.0) as u64).min(len / 2);
                    }
                },
                cx,
            )
        };
        body.child(label("FADES"))
            .child(self.stepper("ed-fin", "Fade in", format!("{:.2} s", c.fade_in_ms as f32 / 1000.0), move |this, d, cx| fade(this, false, d, cx), cx))
            .child(self.stepper("ed-fout", "Fade out", format!("{:.2} s", c.fade_out_ms as f32 / 1000.0), move |this, d, cx| fade(this, true, d, cx), cx))
            .child(hint("From and to black and silence: the picture and its sound together."))
    }

    fn toggle_volume_key(&mut self, clip: Id, cx: &mut Context<Self>) {
        let t = self.playhead;
        self.change(
            |s| {
                let Some(c) = s.clip_mut(clip) else { return };
                let rel = t.saturating_sub(c.start_ms).min(c.len_ms());
                if let Some(i) = c.volume_keys.iter().position(|k| k.at_ms.abs_diff(rel) < 40) {
                    let gone = c.volume_keys.remove(i);
                    if c.volume_keys.is_empty() {
                        c.volume = gone.volume;
                    }
                } else {
                    let volume = c.volume_at(t);
                    c.volume_keys.push(VolumeKey { at_ms: rel, volume });
                    c.volume_keys.sort_by_key(|k| k.at_ms);
                }
            },
            cx,
        );
    }
}

impl Editor {
    /// For the screenshot hook: the first clip's sound detached, faded and keyed, a voice
    /// cleaned, and music (a file next to the workspace) under it.
    pub fn hook_sound(&mut self, cx: &mut Context<Self>) {
        let Some(first) = self.seq().and_then(|s| s.tracks.iter().find(|t| t.kind == TrackKind::Video)).and_then(|t| t.clips.first()).map(|c| c.id) else { return };
        let music = self.workspace.as_ref().and_then(|w| w.parent()).map(|d| d.join("ed-media").join("Music.wav")).and_then(|p| Media::from_file(&p).ok());
        let mut sound = None;
        self.change(
            |s| {
                sound = s.detach_sound(first);
                if let Some(c) = sound.and_then(|id| s.clip_mut(id)) {
                    c.fade_in_ms = 1_500;
                    c.fade_out_ms = 2_000;
                    c.voice = Some(VoiceClean { amount: 0.25, podcast: false });
                }
                if let Some(m) = music {
                    let id = s.add_media(m);
                    if let Some(c) = s.place(id, 2_000, None).and_then(|c| s.clip_mut(c)) {
                        c.volume_keys = vec![VolumeKey { at_ms: 2_000, volume: 1.0 }, VolumeKey { at_ms: 5_000, volume: 0.25 }, VolumeKey { at_ms: 12_000, volume: 0.25 }, VolumeKey { at_ms: 15_000, volume: 1.0 }];
                    }
                }
            },
            cx,
        );
        self.pick_only(sound);
        self.tab = InspectorTab::Audio;
        self.left = LeftTab::Audio;
        self.seek(3_000, cx);
    }
}
