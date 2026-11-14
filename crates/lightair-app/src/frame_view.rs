//! A preview frame as an element. macOS: the IOSurface itself (`surface`), no copy. Elsewhere:
//! the frame's BGRA pixels as an image, until the zero-copy Windows path lands
//! (docs/plans/windows.md, phase 2).

use gpui_kit::*;
use lightair_preview::Frame;

#[cfg(target_os = "macos")]
pub fn frame(f: Frame) -> Surface {
    surface(f)
}

/// Starts what takes unused frame images out of the windows' texture atlases.
#[cfg(target_os = "macos")]
pub fn init(_: &mut App) {}

#[cfg(not(target_os = "macos"))]
thread_local! {
    static GONE: std::cell::OnceCell<futures_channel::mpsc::UnboundedSender<std::sync::Arc<RenderImage>>> =
        const { std::cell::OnceCell::new() };
}

/// Starts what takes unused frame images out of the windows' texture atlases: outside any render,
/// when every window is in the app's list.
#[cfg(not(target_os = "macos"))]
pub fn init(cx: &mut App) {
    let (tx, mut rx) = futures_channel::mpsc::unbounded::<std::sync::Arc<RenderImage>>();
    GONE.with(|g| g.set(tx).ok());
    cx.spawn(async move |cx| {
        use futures_util::StreamExt;
        while let Some(image) = rx.next().await {
            cx.update(|cx| cx.drop_image(image, None));
        }
    })
    .detach();
}

/// Each frame becomes an image once (several views may show the same frame); images no longer
/// shown leave the windows' texture atlases after this render (`init`).
#[cfg(not(target_os = "macos"))]
pub fn frame(f: Frame) -> Img {
    use std::cell::RefCell;
    use std::sync::Arc;

    /// The images made lately: (frame, image, the call that last used it).
    #[derive(Default)]
    struct Cache {
        made: Vec<(Frame, Arc<RenderImage>, u64)>,
        calls: u64,
    }
    /// Calls after which an unused image goes (more than the views shown at once).
    const KEEP: u64 = 64;

    thread_local! {
        static CACHE: RefCell<Cache> = RefCell::default();
    }
    let (image, gone) = CACHE.with_borrow_mut(|c| {
        c.calls += 1;
        let now = c.calls;
        let image = match c.made.iter_mut().find(|(g, ..)| Arc::ptr_eq(g, &f)) {
            Some((_, image, used)) => {
                *used = now;
                image.clone()
            }
            None => {
                let buffer = image::RgbaImage::from_raw(f.width as u32, f.height as u32, f.bgra.clone())
                    .unwrap_or_else(|| image::RgbaImage::new(1, 1));
                let image = Arc::new(RenderImage::new([image::Frame::new(buffer)]));
                c.made.push((f.clone(), image.clone(), now));
                image
            }
        };
        let (keep, gone): (Vec<_>, Vec<_>) = c.made.drain(..).partition(|(.., used)| now - used < KEEP);
        c.made = keep;
        (image, gone)
    });
    GONE.with(|g| {
        if let Some(tx) = g.get() {
            for (_, image, _) in gone {
                let _ = tx.unbounded_send(image);
            }
        }
    });
    img(ImageSource::Render(image))
}
