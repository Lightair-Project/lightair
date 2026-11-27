//! The co-host's voice picker: a sheet over the window, like Add object. The voice vendors on
//! the left (with how many voices each has, or that it needs a key), every voice of the one
//! shown as a tile with its tags and a ▶ to hear it on this computer (never on stream), a search
//! and a gender filter where the vendor says. Choosing one sets the co-host's vendor and voice.

use std::time::Duration;

use gpui_kit::component::Sizable;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{
    Escape, Input as TextInput, InputEvent, InputState, MoveDown, MoveLeft, MoveRight, MoveUp,
};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::*;
use serde_json::json;

use super::view::{CohostEvent, CohostView};
use super::{Host, KeyState, Voices, keys, voices};
use crate::ui::{self, tok};

/// The vendors with voices to pick, in the rail's order.
const VENDORS: &[(&str, &str)] =
    &[("cartesia", "Cartesia"), ("elevenlabs", "ElevenLabs"), ("gemini", "Gemini"), ("system", "System voice")];
/// Tiles a row.
const COLUMNS: usize = 2;

pub struct VoiceSheet {
    host: Host,
    cohost: WeakEntity<CohostView>,
    open: bool,
    /// The vendor whose voices show.
    vendor: &'static str,
    search: Entity<InputState>,
    gender: Option<&'static str>,
    /// The highlighted voice (keys and the pointer move it).
    sel: usize,
    scroll: ScrollHandle,
    /// The search takes the keys when it's next drawn.
    focus_search: bool,
    clearing: bool,
    /// What was last drawn of the voices and previews (redrawn when it changes).
    drawn: String,
    _subs: Vec<Subscription>,
}

impl VoiceSheet {
    pub fn new(host: Host, cohost: WeakEntity<CohostView>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search voices: name, accent, style…"));
        let subs = vec![cx.subscribe_in(&search, window, |this, _, ev: &InputEvent, window, cx| match ev {
            InputEvent::Change if std::mem::take(&mut this.clearing) => {}
            InputEvent::Change => {
                this.sel = 0;
                this.scroll.set_offset(point(px(0.), px(0.)));
                cx.notify();
            }
            InputEvent::PressEnter { .. } => this.choose_highlighted(window, cx),
            _ => {}
        })];
        // a list that arrives or a preview that starts or ends shows while it's open
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(150)).await;
                let ok = this.update(cx, |this, cx| {
                    if !this.open {
                        return;
                    }
                    let now = this.snapshot();
                    if now != this.drawn {
                        this.drawn = now;
                        cx.notify();
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
        VoiceSheet {
            host,
            cohost,
            open: false,
            vendor: "gemini",
            search,
            gender: None,
            sel: 0,
            scroll: ScrollHandle::new(),
            drawn: String::new(),
            focus_search: false,
            clearing: false,
            _subs: subs,
        }
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    fn snapshot(&self) -> String {
        let v = match self.host.voices(self.vendor) {
            Voices::Loading => "loading".to_string(),
            Voices::Ready(l) => format!("ready {}", l.len()),
            Voices::Failed(e) => e,
        };
        let key = format!("{:?}", self.host.key_state(keys::service_for_voice(self.vendor)));
        let err = self.host.0.ai.lock().unwrap_or_else(|e| e.into_inner()).preview_error.clone();
        format!("{v}|{key}|{:?}|{err:?}", self.host.preview_state())
    }

    /// Opens on the co-host's vendor (given by the co-host's sections, which are mid-update when
    /// they open it), its voice highlighted once drawn.
    pub fn open(&mut self, vendor: &str, cx: &mut Context<Self>) {
        self.vendor = VENDORS.iter().map(|v| v.0).find(|v| *v == vendor).unwrap_or("gemini");
        self.open = true;
        self.gender = None;
        self.focus_search = true;
        self.scroll.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        if self.host.preview_state() != (None, None) {
            self.host.stop_preview();
        }
        cx.notify();
    }

    /// The co-host's vendor and voice (Gemini's Puck until another is chosen).
    fn chosen(&self, cx: &App) -> (String, String) {
        let Some(c) = self.cohost.upgrade() else { return ("gemini".into(), "Puck".into()) };
        let c = c.read(cx);
        let vendor = c.setting("/voice/vendor", cx).as_str().unwrap_or("gemini").to_string();
        let mut voice = c.setting("/voice/voice", cx).as_str().unwrap_or("").to_string();
        if voice.is_empty() && vendor == "gemini" {
            voice = "Puck".into();
        }
        (vendor, voice)
    }

    /// The shown vendor's voices that match the search and the filter.
    fn shown(&self, cx: &App) -> Vec<voices::VoiceInfo> {
        let Voices::Ready(list) = self.host.voices(self.vendor) else { return Vec::new() };
        let query = self.search.read(cx).value().trim().to_lowercase();
        list.into_iter()
            .filter(|v| self.gender.is_none_or(|g| v.gender.as_deref() == Some(g)))
            .filter(|v| {
                query.is_empty()
                    || query.split_whitespace().all(|w| {
                        v.name.to_lowercase().contains(w) || v.tags.iter().any(|t| t.to_lowercase().contains(w))
                    })
            })
            .collect()
    }

    fn choose(&mut self, voice: &str, cx: &mut Context<Self>) {
        let vendor = self.vendor;
        if let Some(c) = self.cohost.upgrade() {
            c.update(cx, |c, cx| c.set(json!({"voice": {"vendor": vendor, "voice": voice}}), cx));
        }
        self.close(cx);
    }

    fn choose_highlighted(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        let shown = self.shown(cx);
        if let Some(v) = shown.get(self.sel.min(shown.len().saturating_sub(1))) {
            let id = v.id.clone();
            self.choose(&id, cx);
        }
    }

    fn show_vendor(&mut self, vendor: &'static str, cx: &mut Context<Self>) {
        self.vendor = vendor;
        self.gender = None;
        let chosen = self.chosen(cx);
        self.sel =
            if chosen.0 == vendor { self.shown(cx).iter().position(|v| v.id == chosen.1).unwrap_or(0) } else { 0 };
        self.scroll.set_offset(point(px(0.), px(0.)));
        self.scroll.scroll_to_item(self.sel / COLUMNS);
        cx.notify();
    }

    /// Moves the highlight by `step` voices (a row is `COLUMNS`), bringing it into view.
    fn move_by(&mut self, step: isize, cx: &mut Context<Self>) {
        let count = self.shown(cx).len();
        if count == 0 {
            return;
        }
        self.sel = (self.sel as isize + step).clamp(0, count as isize - 1) as usize;
        self.scroll.scroll_to_item(self.sel / COLUMNS);
        cx.notify();
    }

    fn open_connections(&mut self, cx: &mut Context<Self>) {
        self.close(cx);
        if let Some(c) = self.cohost.upgrade() {
            c.update(cx, |_, cx| cx.emit(CohostEvent::OpenConnections));
        }
    }

    /// A voice: ▶ (a spinner while its sample's made, ■ while it plays), its name, its tags; a ✓
    /// on the co-host's voice.
    fn tile(&self, i: usize, v: &voices::VoiceInfo, current: bool, cx: &mut Context<Self>) -> Stateful<Div> {
        let (loading, playing) = self.host.preview_state();
        let (is_loading, is_playing) =
            (loading.as_deref() == Some(v.id.as_str()), playing.as_deref() == Some(v.id.as_str()));
        let on = i == self.sel;
        let vendor = self.vendor;
        let voice = v.clone();
        let play = Button::new(("voice-sheet-play", i))
            .icon(ui::icon(if is_playing {
                "square"
            } else if is_loading {
                "loader"
            } else {
                "play"
            }))
            .small()
            .when(is_playing || is_loading, |b| b.primary())
            .when(!(is_playing || is_loading), |b| b.outline())
            .tooltip(if is_playing { "Stop" } else { "Hear it (on this computer only)" })
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                if is_playing {
                    this.host.stop_preview();
                } else {
                    this.host.preview_voice(vendor, voice.clone());
                }
                cx.notify();
            }));
        let tags = if v.tags.is_empty() { v.detail.clone() } else { v.tags.join(" · ") };
        let id = v.id.clone();
        div()
            .id(("voice-sheet-voice", i))
            .flex()
            .flex_1()
            .min_w_0()
            .items_center()
            .gap(px(10.))
            .min_h(px(54.))
            .px(px(8.))
            .py(px(7.))
            .rounded(px(8.))
            .border_1()
            .cursor_pointer()
            .when(on, |d| d.bg(tok::bg_raised()).border_color(tok::border_strong()))
            .when(!on, |d| d.border_color(transparent_black()))
            .child(play)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(1.))
                    .child(
                        div()
                            .text_size(px(13.))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(tok::text())
                            .overflow_hidden()
                            .text_ellipsis()
                            .whitespace_nowrap()
                            .child(v.name.clone()),
                    )
                    .when(!tags.is_empty(), |d| {
                        d.child(
                            div()
                                .text_size(px(11.5))
                                .line_height(px(15.))
                                .text_color(tok::text_tertiary())
                                .line_clamp(2)
                                .child(tags),
                        )
                    }),
            )
            .when(current, |d| d.child(ui::icon("check").size(px(15.)).text_color(tok::ok()).flex_none()))
            .on_mouse_move(cx.listener(move |this, _: &MouseMoveEvent, _, cx| {
                if this.sel != i {
                    this.sel = i;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, _, cx| this.choose(&id, cx)))
    }
}

impl Render for VoiceSheet {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.open {
            return div().into_any_element();
        }
        if std::mem::take(&mut self.focus_search) {
            // last time's search is cleared (its change doesn't move the highlight off the voice)
            let clear = !self.search.read(cx).value().is_empty();
            self.clearing = clear;
            self.search.update(cx, |s, cx| {
                if clear {
                    s.set_value("", window, cx);
                }
                s.focus(window, cx);
            });
            let (vendor, chosen) = self.chosen(cx);
            self.sel =
                if vendor == self.vendor { self.shown(cx).iter().position(|v| v.id == chosen).unwrap_or(0) } else { 0 };
            self.scroll.scroll_to_item(self.sel / COLUMNS);
        }
        let (chosen_vendor, chosen) = self.chosen(cx);

        // the rail: each vendor, with how many voices it has or that it needs a key
        let mut rail =
            div().flex().flex_col().flex_none().w(px(184.)).p_2().gap(px(2.)).border_r_1().border_color(tok::border());
        for &(id, name) in VENDORS {
            let on = self.vendor == id;
            let service = keys::service_for_voice(id);
            let note = match (self.host.key_state(service), id) {
                (KeyState::Missing, v) if v != "system" => "No key".to_string(),
                _ => match self.host.voices(id) {
                    Voices::Ready(l) => l.len().to_string(),
                    Voices::Loading => "…".to_string(),
                    Voices::Failed(_) => "!".to_string(),
                },
            };
            rail = rail.child(
                div()
                    .id(SharedString::from(format!("voice-sheet-vendor-{id}")))
                    .flex()
                    .items_center()
                    .gap_2()
                    .h(px(30.))
                    .px_2()
                    .rounded(px(6.))
                    .cursor_pointer()
                    .text_size(px(13.))
                    .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                    .when(!on, |d| {
                        d.text_color(tok::text_secondary()).hover(|d| d.bg(tok::bg_raised()).text_color(tok::text()))
                    })
                    .child(ui::icon("audio-lines").size(px(15.)))
                    .child(div().flex_1().child(name))
                    .when(chosen_vendor == id, |d| d.child(div().size(px(6.)).rounded_full().bg(tok::ok())))
                    .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(note))
                    .on_click(cx.listener(move |this, _, _, cx| this.show_vendor(id, cx))),
            );
        }
        rail = rail.child(div().flex_1()).child(
            div()
                .px_2()
                .py_1p5()
                .text_size(px(11.))
                .line_height(px(15.))
                .text_color(tok::text_tertiary())
                .child("▶ plays a short sample on this computer, never on stream."),
        );

        // the voices
        let voices = self.host.voices(self.vendor);
        let service = keys::service_for_voice(self.vendor);
        let no_key = self.vendor != "system" && self.host.key_state(service) == KeyState::Missing;
        let all = match &voices {
            Voices::Ready(l) => l.clone(),
            _ => Vec::new(),
        };
        let shown = self.shown(cx);
        self.sel = self.sel.min(shown.len().saturating_sub(1));
        let has_gender = all.iter().any(|v| v.gender.is_some());
        let preview_error = self.host.0.ai.lock().unwrap_or_else(|e| e.into_inner()).preview_error.clone();

        let mut top = div().flex().flex_none().items_center().gap_3().px_3().pt_3().pb_1();
        if has_gender {
            let mut filters = div()
                .flex()
                .gap(px(2.))
                .p(px(2.))
                .rounded(px(7.))
                .bg(tok::bg_app())
                .border_1()
                .border_color(tok::border());
            for (i, (label, g)) in
                [("All", None), ("Female", Some("female")), ("Male", Some("male"))].into_iter().enumerate()
            {
                let on = self.gender == g;
                filters = filters.child(
                    div()
                        .id(("voice-sheet-gender", i))
                        .px_3()
                        .h(px(24.))
                        .flex()
                        .items_center()
                        .rounded(px(5.))
                        .cursor_pointer()
                        .text_size(px(12.))
                        .when(on, |d| d.bg(tok::bg_selected()).text_color(tok::text()))
                        .when(!on, |d| d.text_color(tok::text_secondary()).hover(|d| d.bg(tok::bg_raised())))
                        .child(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.gender = g;
                            this.sel = 0;
                            this.scroll.set_offset(point(px(0.), px(0.)));
                            cx.notify();
                        })),
                );
            }
            top = top.child(filters);
        }
        top = top.child(div().flex_1()).when(!all.is_empty(), |d| {
            d.child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(format!(
                "{} of {} voices",
                shown.len(),
                all.len()
            )))
        });

        let mut list = div()
            .id("voice-sheet-list")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .px_3()
            .pb_3()
            .gap(px(4.));
        let empty = |icon: &str, title: String, detail: Option<String>| {
            div()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .gap_2()
                .py(px(64.))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(40.))
                        .rounded(px(10.))
                        .bg(tok::bg_raised())
                        .child(ui::icon(icon).size(px(18.)).text_color(tok::text_tertiary())),
                )
                .child(div().text_size(px(13.)).text_color(tok::text_secondary()).child(title))
                .children(detail.map(|d| {
                    div().max_w(px(380.)).text_center().text_size(px(12.)).text_color(tok::text_tertiary()).child(d)
                }))
        };
        if no_key && (self.vendor != "gemini" || matches!(voices, Voices::Failed(_))) {
            let name = keys::name_of(service);
            list = list.child(
                empty(
                    "key-round",
                    format!("Add your {name} key to see its voices"),
                    Some("Keys live in Settings → Connections, in the system's credential store.".into()),
                )
                .child(
                    Button::new("voice-sheet-connections")
                        .label("Open Connections")
                        .small()
                        .primary()
                        .on_click(cx.listener(|this, _, _, cx| this.open_connections(cx))),
                ),
            );
        } else {
            match &voices {
                Voices::Loading => list = list.child(empty("loader", "Loading voices…".into(), None)),
                Voices::Failed(e) => {
                    list = list.child(empty("circle-alert", "Voices unavailable".into(), Some(e.clone())))
                }
                Voices::Ready(_) if shown.is_empty() => {
                    let q = self.search.read(cx).value().trim().to_string();
                    list = list.child(empty(
                        "search",
                        if q.is_empty() { "No voices".into() } else { format!("Nothing matches “{q}”") },
                        Some("Try an accent, an age or a style: “british”, “young”, “calm”.".into()),
                    ));
                }
                Voices::Ready(_) => {
                    for (r, row) in shown.chunks(COLUMNS).enumerate() {
                        let mut line = div().flex().gap(px(4.));
                        for (k, v) in row.iter().enumerate() {
                            let i = r * COLUMNS + k;
                            let current = chosen_vendor == self.vendor && v.id == chosen;
                            line = line.child(self.tile(i, v, current, cx));
                        }
                        for _ in row.len()..COLUMNS {
                            line = line.child(div().flex_1());
                        }
                        list = list.child(line);
                    }
                }
            }
        }

        let header = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_2()
            .h(px(48.))
            .px_3()
            .border_b_1()
            .border_color(tok::border())
            .child(div().flex_none().text_size(px(14.)).font_weight(FontWeight::SEMIBOLD).child("Voice"))
            .child(div().flex_none().w(px(1.)).h(px(18.)).mx_1().bg(tok::border()))
            .child(
                div().flex_1().min_w_0().child(
                    TextInput::new(&self.search)
                        .appearance(false)
                        .prefix(ui::icon("search").size(px(15.)).text_color(tok::text_tertiary()))
                        .cleanable(true),
                ),
            )
            .child(key_cap("esc"));
        let footer = div()
            .flex()
            .flex_none()
            .items_center()
            .gap_4()
            .h(px(34.))
            .px_3()
            .border_t_1()
            .border_color(tok::border())
            .bg(tok::bg_app().opacity(0.35))
            .child(key_hint("↑ ↓ ← →", "Move"))
            .child(key_hint("↵", "Choose"))
            .child(key_hint("esc", "Close"))
            .child(div().flex_1())
            .children(
                preview_error.map(|e| div().min_w_0().truncate().text_size(px(11.)).text_color(tok::error()).child(e)),
            );

        div()
            .absolute()
            .inset_0()
            .flex()
            .flex_col()
            .items_center()
            .pt(px(72.))
            .pb(px(24.))
            .bg(tok::scrim())
            .occlude()
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| this.close(cx)))
            .child(
                div()
                    .id("voice-sheet")
                    .flex()
                    .flex_col()
                    .w(px(760.))
                    .h(px(560.))
                    .flex_shrink(1.)
                    .min_h(px(320.))
                    .overflow_hidden()
                    .rounded(px(12.))
                    .bg(tok::bg_panel())
                    .border_1()
                    .border_color(tok::border_strong())
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    // the search has the keys and turns them into its actions: the sheet takes them
                    // first (← → only while the search is empty, so they still move its cursor)
                    .capture_action(cx.listener(|this, _: &Escape, _, cx| {
                        cx.stop_propagation();
                        this.close(cx);
                    }))
                    .capture_action(cx.listener(|this, _: &MoveUp, _, cx| {
                        cx.stop_propagation();
                        this.move_by(-(COLUMNS as isize), cx);
                    }))
                    .capture_action(cx.listener(|this, _: &MoveDown, _, cx| {
                        cx.stop_propagation();
                        this.move_by(COLUMNS as isize, cx);
                    }))
                    .capture_action(cx.listener(|this, _: &MoveLeft, _, cx| {
                        if this.search.read(cx).value().is_empty() {
                            cx.stop_propagation();
                            this.move_by(-1, cx);
                        }
                    }))
                    .capture_action(cx.listener(|this, _: &MoveRight, _, cx| {
                        if this.search.read(cx).value().is_empty() {
                            cx.stop_propagation();
                            this.move_by(1, cx);
                        }
                    }))
                    .child(header)
                    .child(
                        div()
                            .flex()
                            .flex_1()
                            .min_h_0()
                            .child(rail)
                            .child(div().flex().flex_col().flex_1().min_w_0().child(top).child(list)),
                    )
                    .child(footer),
            )
            .into_any_element()
    }
}

/// A key, drawn as a key cap.
fn key_cap(keys: &'static str) -> Div {
    div()
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .min_w(px(20.))
        .h(px(20.))
        .px_1p5()
        .rounded(px(4.))
        .bg(tok::bg_raised())
        .border_1()
        .border_color(tok::border())
        .text_size(px(10.5))
        .text_color(tok::text_secondary())
        .child(keys)
}

/// A key and what it does.
fn key_hint(keys: &'static str, label: &'static str) -> Div {
    div()
        .flex()
        .items_center()
        .gap_1p5()
        .child(key_cap(keys))
        .child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(label))
}
