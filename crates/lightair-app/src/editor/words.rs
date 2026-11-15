//! The inspector's Words tab: a clip's speech transcribed on the computer, its words to read
//! and cut (click to mark), and its filler words and long pauses taken out in one go. What's
//! cut closes up on its track.

use super::*;
use lightair_render::sequence::Clip;
use lightair_render::speech;

impl Editor {
    /// Transcribes a medium's sound (on a thread).
    fn transcribe(&mut self, media: &Media, cx: &mut Context<Self>) {
        if self.transcribing.is_some() {
            return;
        }
        let files = if media.audio.is_empty() { vec![media.path.clone()] } else { media.audio.clone() };
        let (id, tx) = (media.id, self.tx.clone());
        self.transcribing = Some(id);
        self.note = Some(format!("Listening to “{}”…", media.name).into());
        std::thread::spawn(move || {
            let r = speech::transcribe(&files, &looks::model()).map_err(|e| e.to_string());
            let _ = tx.send(Done::Transcript(id, r));
        });
        cx.notify();
    }

    pub(super) fn take_transcript(&mut self, media: Id, r: Result<speech::Transcript, String>, cx: &mut Context<Self>) {
        self.transcribing = None;
        match r {
            Ok(t) => {
                let n = t.words.len();
                self.change(|s| if let Some(m) = s.media.iter_mut().find(|m| m.id == media) { m.transcript = Some(t) }, cx);
                self.note = Some(format!("{n} words heard").into());
            }
            Err(e) => self.note = Some(format!("It couldn't be transcribed: {e}").into()),
        }
        cx.notify();
    }

    /// Cuts source ranges out of a clip.
    fn cut_words(&mut self, clip: Id, ranges: Vec<(u64, u64)>, cx: &mut Context<Self>) {
        let ranges = speech::merge(ranges);
        if ranges.is_empty() {
            self.note = Some("Nothing to take out.".into());
            cx.notify();
            return;
        }
        let mut removed = 0;
        self.change(|s| removed = s.cut_out(clip, &ranges), cx);
        self.marked.clear();
        self.note = Some(format!("{:.1} s taken out", removed as f32 / 1000.0).into());
        cx.notify();
    }

    pub(super) fn words_section(&self, c: &Clip, m: &Media, mut body: Div, cx: &mut Context<Self>) -> Div {
        let cid = c.id;
        let Some(t) = &m.transcript else {
            let mid = m.clone();
            let action = if self.transcribing.is_some() {
                Button::new("ed-words").label("Listening…").xsmall().outline().loading(true).into_any_element()
            } else if self.downloading {
                Button::new("ed-words").label("Downloading the speech model…").xsmall().outline().loading(true).into_any_element()
            } else if !looks::model().is_file() {
                Button::new("ed-words")
                    .label("Get speech model")
                    .icon(ui::icon("download"))
                    .xsmall()
                    .outline()
                    .tooltip("142 MB, once: speech is recognised on this computer, never sent anywhere")
                    .on_click(cx.listener(|this, _, _, cx| this.download_model(cx)))
                    .into_any_element()
            } else {
                Button::new("ed-words")
                    .label("Transcribe")
                    .icon(ui::icon("captions"))
                    .xsmall()
                    .primary()
                    .on_click(cx.listener(move |this, _, _, cx| this.transcribe(&mid, cx)))
                    .into_any_element()
            };
            return body
                .child(label("WORDS"))
                .child(hint("Its speech as words: read it, cut words out, take out fillers and long pauses."))
                .child(div().flex().child(action));
        };
        let (from, to) = (c.in_ms, c.out_ms);
        let now = c.source_at(self.playhead);
        let tighten = speech::tightening(t, from, to, 900, 300);
        let gain: u64 = tighten.iter().map(|(a, b)| b - a).sum();
        let marked: Vec<(u64, u64)> = self.marked.iter().filter(|(cl, ..)| *cl == cid).map(|(_, a, b)| (*a, *b)).collect();
        let mut words = div().flex().flex_wrap().gap_x_1().gap_y_0p5().text_size(px(13.)).line_height(px(20.));
        for (n, w) in t.words.iter().enumerate().filter(|(_, w)| w.end_ms > from && w.start_ms < to) {
            let (a, b) = (w.start_ms, w.end_ms);
            let cut = marked.contains(&(a, b));
            let here = now.is_some_and(|s| s >= a && s < b);
            let filler = speech::filler(t, &w.text);
            words = words.child(
                div()
                    .id(("ed-w", n))
                    .px_0p5()
                    .rounded(px(3.))
                    .cursor_pointer()
                    .when(filler, |d| d.italic().text_color(tok::text_tertiary()))
                    .when(cut, |d| d.line_through().bg(rgba(0xEF444440)).text_color(tok::text_tertiary()))
                    .when(here, |d| d.bg(tok::bg_selected()))
                    .hover(|d| d.bg(tok::bg_selected()))
                    .child(w.text.clone())
                    .on_click(cx.listener(move |this, _, _, cx| {
                        // marked to cut, or not; the playhead goes to it
                        match this.marked.iter().position(|m| *m == (cid, a, b)) {
                            Some(i) => {
                                this.marked.remove(i);
                            }
                            None => this.marked.push((cid, a, b)),
                        }
                        if let Some(c) = this.seq().and_then(|s| s.clip(cid)) {
                            let at = c.start_ms + ((a.max(c.in_ms) - c.in_ms) as f32 / c.speed.max(0.05)) as u64;
                            this.seek(at, cx);
                        }
                        cx.notify();
                    })),
            );
        }
        let n = marked.len();
        body = body
            .child(label("WORDS"))
            .child(
                div()
                    .flex()
                    .flex_wrap()
                    .gap_1()
                    .child(
                        Button::new("ed-tighten")
                            .label(format!("Remove fillers and pauses ({:.1} s)", gain as f32 / 1000.0))
                            .icon(ui::icon("wand-sparkles"))
                            .xsmall()
                            .outline()
                            .on_click(cx.listener(move |this, _, _, cx| this.cut_words(cid, tighten.clone(), cx))),
                    )
                    .when(n > 0, |d| {
                        let ranges = marked.clone();
                        d.child(
                            Button::new("ed-cutwords")
                                .label(format!("Cut {n} marked"))
                                .icon(ui::icon("scissors"))
                                .xsmall()
                                .primary()
                                .on_click(cx.listener(move |this, _, _, cx| this.cut_words(cid, ranges.clone(), cx))),
                        )
                    }),
            )
            .child(hint("Click words to mark them for cutting. Fillers are in italics. The clips after this one on its track move up."))
            .child(div().id("ed-words-list").max_h(px(320.)).overflow_y_scroll().p_2().rounded(px(6.)).bg(tok::bg_app()).child(words));
        body
    }
}
