//! Right-click menus: on a clip (on the timeline or in the player), a track's header, the
//! timeline's empty space and a media card. Right-clicking a clip picks it first (unless it's
//! one of several picked), then offers what can be done to what's picked. Items run what the
//! keys and the tool bar run, and show the keys.

use gpui_kit::component::menu::PopupMenu;
use lightair_render::sequence::Track;

use super::*;

type Ed = WeakEntity<Editor>;

/// A key as it reads on this system ("⌘C", or "Ctrl+C").
fn cmd(k: &str) -> String {
    if cfg!(target_os = "macos") { format!("⌘{k}") } else { format!("Ctrl+{k}") }
}

/// An item: its label, the key that does the same (if one does), what it does.
fn entry(ed: &Ed, label: &'static str, icon: &'static str, keys: impl Into<String>, disabled: bool, f: impl Fn(&mut Editor, &mut Window, &mut Context<Editor>) + 'static) -> PopupMenuItem {
    let keys: SharedString = keys.into().into();
    let ed = ed.clone();
    PopupMenuItem::element(move |_, _| {
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_between()
            .gap_6()
            .child(label)
            .when(!keys.is_empty(), |d| d.child(div().text_size(px(11.)).text_color(tok::text_tertiary()).child(keys.clone())))
    })
    .icon(ui::icon(icon))
    .disabled(disabled)
    .on_click(move |_, w, cx| {
        if let Some(e) = ed.upgrade() {
            e.update(cx, |e, cx| f(e, w, cx));
        }
    })
}

/// What a clip menu needs to know about what's picked.
struct Facts {
    count: usize,
    linked: bool,
    detachable: Option<Id>,
    muted: bool,
    path: Option<PathBuf>,
    pasting: bool,
}

impl Editor {
    fn facts(&self) -> Option<Facts> {
        let seq = self.seq()?;
        let picked = self.picked_clips();
        let first = seq.clip(*picked.first()?)?;
        let media = seq.media(first.media);
        let on_video = seq.find(first.id).is_some_and(|(t, _)| seq.tracks[t].kind == TrackKind::Video);
        let linked = picked.iter().any(|c| seq.linked(*c).len() > 1);
        Some(Facts {
            count: picked.len(),
            linked,
            detachable: (picked.len() == 1 && on_video && !linked && !first.muted && media.is_some_and(|m| !m.audio.is_empty())).then_some(first.id),
            muted: picked.iter().filter_map(|c| seq.clip(*c)).all(|c| c.muted),
            path: media.filter(|_| picked.len() == 1 && first.content.is_none()).map(|m| m.path.clone()),
            pasting: !self.clipboard.is_empty(),
        })
    }

    /// A right-press on a clip: it's picked (kept with the others if it's one of them).
    pub(super) fn menu_pick(&mut self, clip: Id, cx: &mut Context<Self>) {
        if !self.is_picked(clip) {
            self.picked = if self.linkage { self.seq().map(|s| s.linked(clip)).unwrap_or_else(|| vec![clip]) } else { vec![clip] };
            self.selected = Some(clip);
            cx.notify();
        }
    }

    /// A right-press on the player: the picture under it, picked.
    pub(super) fn menu_pick_stage(&mut self, pos: Point<Pixels>, cx: &mut Context<Self>) {
        match self.to_result(pos).and_then(|p| self.picture_at(p)) {
            Some(q) => self.menu_pick(q.clip, cx),
            None => {
                self.pick_only(None);
                cx.notify();
            }
        }
    }

    /// The picked clips linked together (one link), or their links undone.
    fn set_linked(&mut self, link: bool, cx: &mut Context<Self>) {
        let picked = if link { self.picked_clips() } else { self.picked_with_links() };
        let id = link.then(Id::new);
        self.change(|s| picked.iter().for_each(|c| if let Some(c) = s.clip_mut(*c) { c.link = id }), cx);
    }

    fn set_muted(&mut self, muted: bool, cx: &mut Context<Self>) {
        let picked = self.picked_with_links();
        self.change(|s| picked.iter().for_each(|c| if let Some(c) = s.clip_mut(*c) { c.muted = muted }), cx);
    }

    fn add_track(&mut self, kind: TrackKind, cx: &mut Context<Self>) {
        self.change(
            |s| match kind {
                TrackKind::Video => s.tracks.insert(0, Track::empty(kind)),
                TrackKind::Audio => s.tracks.push(Track::empty(kind)),
            },
            cx,
        );
    }

    /// A track and its clips taken away (a kind's last track stays, emptied).
    fn delete_track(&mut self, ti: usize, cx: &mut Context<Self>) {
        self.pick_only(None);
        self.change(
            |s| {
                let Some(kind) = s.tracks.get(ti).map(|t| t.kind) else { return };
                if s.tracks.iter().filter(|t| t.kind == kind).count() > 1 {
                    s.tracks.remove(ti);
                } else {
                    s.tracks[ti].clips.clear();
                }
            },
            cx,
        );
    }
}

/// On a clip (or what's picked).
pub(super) fn clip_menu(menu: PopupMenu, ed: &Ed, cx: &mut Context<PopupMenu>) -> PopupMenu {
    let Some(f) = ed.upgrade().and_then(|e| e.read(cx).facts()) else { return blank_menu(menu, ed, cx) };
    let mut menu = menu
        .item(entry(ed, "Cut", "scissors", cmd("X"), false, |e, _, cx| e.cut_picked(cx)))
        .item(entry(ed, "Copy", "copy", cmd("C"), false, |e, _, cx| e.copy_picked(cx)))
        .item(entry(ed, "Paste", "clipboard-paste", cmd("V"), !f.pasting, |e, _, cx| e.paste_at(None, cx)))
        .item(entry(ed, "Duplicate", "copy-plus", cmd("D"), false, |e, _, cx| e.duplicate_picked(cx)))
        .item(entry(ed, "Delete", "trash", "⌫", false, |e, _, cx| e.delete_selected(cx)))
        .separator()
        .item(entry(ed, "Split at playhead", "scissors-line-dashed", cmd("B"), false, |e, _, cx| e.split(cx)))
        .item(entry(ed, "Delete left of playhead", "arrow-left-to-line", "Q", false, |e, _, cx| e.delete_side(true, cx)))
        .item(entry(ed, "Delete right of playhead", "arrow-right-to-line", "W", false, |e, _, cx| e.delete_side(false, cx)))
        .separator();
    if let Some(c) = f.detachable {
        menu = menu.item(entry(ed, "Detach sound", "unlink", "", false, move |e, _, cx| {
            let mut made = None;
            e.change(|s| made = s.detach_sound(c), cx);
            if made.is_some() {
                e.pick_only(made);
            }
        }));
    }
    if f.linked {
        menu = menu.item(entry(ed, "Unlink", "unlink", "", false, |e, _, cx| e.set_linked(false, cx)));
    } else if f.count > 1 {
        menu = menu.item(entry(ed, "Link", "link", "", false, |e, _, cx| e.set_linked(true, cx)));
    }
    menu = menu
        .item(entry(ed, if f.muted { "Unmute" } else { "Mute" }, if f.muted { "volume-2" } else { "volume-x" }, "", false, move |e, _, cx| e.set_muted(!f.muted, cx)))
        .separator()
        .item(entry(ed, "Freeze frame", "snowflake", "", false, |e, _, cx| e.freeze(cx)))
        .item(entry(ed, "Reverse", "rewind", "", false, |e, _, cx| e.reverse(cx)))
        .item(entry(ed, "Mirror", "flip-horizontal-2", "", false, |e, _, cx| e.mirror(cx)))
        .item(entry(ed, "Rotate 90°", "rotate-cw", "", false, |e, _, cx| e.rotate(cx)));
    if let Some(path) = f.path {
        menu = menu.separator().item(entry(ed, if cfg!(windows) { "Show in Explorer" } else { "Show in Finder" }, "folder", "", false, move |_, _, _| {
            let _ = lightair_platform::files::reveal(&path);
        }));
    }
    menu
}

/// On the timeline's empty space (where it was pressed: `Editor::menu_at`).
pub(super) fn blank_menu(menu: PopupMenu, ed: &Ed, cx: &mut Context<PopupMenu>) -> PopupMenu {
    let Some(e) = ed.upgrade() else { return menu };
    let pasting = !e.read(cx).clipboard.is_empty();
    menu.item(entry(ed, "Paste here", "clipboard-paste", cmd("V"), !pasting, |e, _, cx| {
        let at = e.menu_at.or(Some(e.playhead));
        e.paste_at(at, cx)
    }))
    .item(entry(ed, "Select all", "box-select", cmd("A"), false, |e, _, cx| e.select_all(cx)))
    .separator()
    .item(entry(ed, "Add marker at playhead", "flag", "M", false, |e, _, cx| e.toggle_marker(cx)))
    .item(entry(ed, "Zoom to fit", "scan", "⇧0", false, |e, _, cx| e.zoom_to_fit(cx)))
    .separator()
    .item(entry(ed, "Add video track", "film", "", false, |e, _, cx| e.add_track(TrackKind::Video, cx)))
    .item(entry(ed, "Add audio track", "music", "", false, |e, _, cx| e.add_track(TrackKind::Audio, cx)))
}

/// On a track's header.
pub(super) fn track_menu(menu: PopupMenu, ed: &Ed, ti: usize, cx: &mut Context<PopupMenu>) -> PopupMenu {
    let Some(track) = ed.upgrade().and_then(|e| e.read(cx).seq().and_then(|s| s.tracks.get(ti)).cloned()) else { return menu };
    let toggle = move |f: fn(&mut Track)| move |e: &mut Editor, _: &mut Window, cx: &mut Context<Editor>| e.change(|s| if let Some(t) = s.tracks.get_mut(ti) { f(t) }, cx);
    let mut menu = menu
        .item(entry(ed, "Add video track", "film", "", false, |e, _, cx| e.add_track(TrackKind::Video, cx)))
        .item(entry(ed, "Add audio track", "music", "", false, |e, _, cx| e.add_track(TrackKind::Audio, cx)))
        .separator();
    menu = match track.kind {
        TrackKind::Video => menu.item(entry(ed, if track.hidden { "Show track" } else { "Hide track" }, if track.hidden { "eye" } else { "eye-off" }, "", false, toggle(|t| t.hidden = !t.hidden))),
        TrackKind::Audio => menu.item(entry(ed, if track.muted { "Unmute track" } else { "Mute track" }, if track.muted { "volume-2" } else { "volume-x" }, "", false, toggle(|t| t.muted = !t.muted))),
    };
    menu.item(entry(ed, if track.locked { "Unlock track" } else { "Lock track" }, if track.locked { "lock-open" } else { "lock" }, "", false, toggle(|t| t.locked = !t.locked)))
        .item(entry(ed, "Reset height", "move-vertical", "", track.height.is_none(), toggle(|t| t.height = None)))
        .separator()
        .item(entry(ed, "Delete track", "trash", "", false, move |e, _, cx| e.delete_track(ti, cx)))
}

/// On a media card.
pub(super) fn media_menu(menu: PopupMenu, ed: &Ed, source: MediaSource, cx: &mut Context<PopupMenu>) -> PopupMenu {
    let Some(e) = ed.upgrade() else { return menu };
    let seq = e.read(cx).seq().cloned();
    let (s1, s2) = (source.clone(), source.clone());
    let mut menu = menu
        .item(entry(ed, "Add to timeline", "plus", "", false, move |e, _, cx| e.add(s1.clone(), None, None, cx)))
        .item(entry(ed, "Add at playhead", "arrow-down-to-line", "", false, move |e, _, cx| {
            let at = Some(e.playhead);
            e.add(s2.clone(), at, None, cx)
        }));
    let path = match &source {
        MediaSource::Item(item) => {
            if item.kind == Kind::StudioTake {
                let studio_take = item.clone();
                menu = menu.item(entry(ed, "Open parts", "layers", "", false, move |e, _, cx| e.open_studio_take(studio_take.clone(), cx)));
            }
            item.files.first().map(|f| f.path.clone())
        }
        MediaSource::Media(id) => seq.as_ref().and_then(|s| s.media(*id)).map(|m| m.path.clone()),
        MediaSource::Part(p) => p.media.as_ref().map(|m| m.path.clone()),
        MediaSource::Content(..) => None,
    };
    if let Some(path) = path {
        menu = menu.separator().item(entry(ed, if cfg!(windows) { "Show in Explorer" } else { "Show in Finder" }, "folder", "", false, move |_, _, _| {
            let _ = lightair_platform::files::reveal(&path);
        }));
    }
    if let MediaSource::Media(id) = source {
        let used = seq.as_ref().is_some_and(|s| s.tracks.iter().flat_map(|t| &t.clips).any(|c| c.media == id));
        menu = menu.item(entry(ed, "Remove from project", "trash", "", used, move |e, _, cx| e.change(|s| s.media.retain(|m| m.id != id), cx)));
    }
    menu
}
