//! What's inside a Studio take, on its Library card: its parts (the screen or camera, each
//! layer of its scene, each recorded mixer track), each sent to the Editor on its own or
//! exported as a file.

use super::*;
use lightair_render::sequence::{Media, TakePart};

/// A take item's folder.
pub(super) fn take_folder(item: &Item) -> Option<PathBuf> {
    item.files.iter().find_map(|f| f.path.parent().filter(|p| p.extension().is_some_and(|e| e == "lightair-take")).map(Path::to_path_buf))
}

/// A part's icon.
pub(crate) fn part_icon(p: &TakePart) -> &'static str {
    match p.what() {
        "Screen" => "monitor",
        "Video" => "video",
        "Sound" => "audio-lines",
        "Picture" => "image",
        "Text" => "type",
        "Colour" => "square",
        _ => "file",
    }
}

impl Library {
    /// Opens or closes a take's parts (read on a thread the first time).
    pub(super) fn toggle_inside(&mut self, item: &Item, cx: &mut Context<Self>) {
        let id = item.id;
        if !self.inside_open.remove(&id) {
            self.inside_open.insert(id);
        }
        if !self.inside.contains_key(&id)
            && let Some(folder) = take_folder(item)
        {
            self.inside.insert(id, None);
            let title = item.title.clone();
            cx.spawn(async move |this, cx| {
                let parts = cx
                    .background_executor()
                    .spawn(async move {
                        let mut parts = Media::take_layout(&folder);
                        for p in parts.iter_mut() {
                            if let Some(m) = &mut p.media {
                                m.item = Some(id);
                                m.name = if m.take.is_some() { title.clone() } else { format!("{title} · {}", p.name) };
                            }
                        }
                        parts
                    })
                    .await;
                let _ = this.update(cx, |this, cx| {
                    this.inside.insert(id, Some(parts));
                    cx.notify();
                });
            })
            .detach();
        }
        cx.notify();
    }

    /// Copies a part's file into a folder the user picks.
    fn export_part(&mut self, part: TakePart, cx: &mut Context<Self>) {
        let Some(file) = part.file().map(Path::to_path_buf) else { return };
        let picked = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Export here".into()) });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(dirs))) = picked.await else { return };
            let Some(dir) = dirs.into_iter().next() else { return };
            let ext = file.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
            let base = part.name.replace(['/', ':', '\\'], "-");
            let out = (1..)
                .map(|n| if n == 1 { dir.join(format!("{base}.{ext}")) } else { dir.join(format!("{base} {n}.{ext}")) })
                .find(|p| !p.exists())
                .unwrap_or_else(|| dir.join(format!("{base}.{ext}")));
            let r = cx.background_executor().spawn(async move { std::fs::copy(&file, &out).map(|_| out) }).await;
            let _ = this.update(cx, |this, cx| {
                this.note = Some(match r {
                    Ok(p) => format!("Exported {}", p.display()).into(),
                    Err(e) => format!("It couldn't be exported: {e}").into(),
                });
                cx.notify();
            });
        })
        .detach();
    }

    /// The parts list under a take's card.
    pub(super) fn inside_list(&self, item: &Item, n: usize, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.inside_open.contains(&item.id) {
            return None;
        }
        let Some(parts) = self.inside.get(&item.id).and_then(Option::as_ref) else {
            return Some(div().px_1().text_size(px(12.)).text_color(tok::text_tertiary()).child("Reading the take…").into_any_element());
        };
        let mut list = div().flex().flex_col().gap_0p5().p_1().rounded(px(8.)).bg(tok::bg_panel()).border_1().border_color(tok::border());
        for (k, p) in parts.iter().enumerate() {
            let id = n * 64 + k;
            let (send, export) = (p.clone(), p.clone());
            let has_file = p.file().is_some();
            list = list.child(
                div()
                    .id(("lib-part", id))
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .px_1p5()
                    .py_1()
                    .rounded(px(6.))
                    .hover(|d| d.bg(tok::bg_raised()))
                    .child(ui::icon(part_icon(p)).size(px(13.)).text_color(tok::text_secondary()))
                    .child(div().flex_1().min_w_0().overflow_hidden().text_ellipsis().whitespace_nowrap().text_size(px(12.)).child(p.name.clone()))
                    .child(div().text_size(px(10.)).text_color(tok::text_tertiary()).child(p.what()))
                    .child(ui::icon_button(("lib-part-use", id), "scissors", tok::text_secondary()).tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new("Use in the Editor").build(w, cx)).on_click(cx.listener(
                        move |this, _, _, cx| {
                            this.open_parts = Some(vec![send.clone()]);
                            cx.notify();
                        },
                    )))
                    .when(has_file, |d| {
                        d.child(ui::icon_button(("lib-part-exp", id), "download", tok::text_secondary()).tooltip(|w, cx| gpui_kit::component::tooltip::Tooltip::new("Export its file…").build(w, cx)).on_click(
                            cx.listener(move |this, _, _, cx| this.export_part(export.clone(), cx)),
                        ))
                    }),
            );
        }
        Some(list.into_any_element())
    }
}

impl Library {
    /// For the screenshot hook: the newest take's parts shown.
    pub(crate) fn hook_inside(&mut self, cx: &mut Context<Self>) {
        self.reload(cx);
        let take = self.indexes.values().flat_map(|i| i.newest_first().into_iter().cloned().collect::<Vec<_>>()).find(|i| i.kind == Kind::Take);
        if let Some(item) = take {
            self.toggle_inside(&item, cx);
        }
    }
}
