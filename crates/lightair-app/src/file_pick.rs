//! Open panels that show only the files a choice can take, and say what to choose. GPUI's own
//! panel takes any file; on macOS this is AppKit's `NSOpenPanel` as a sheet on the window, with
//! files of other kinds greyed out (a Live2D model is a `.model3.json`, so the test is on the
//! name's end, not the extension). Elsewhere it's GPUI's panel, and a file of another kind is
//! turned away after.

use std::path::PathBuf;

use futures_channel::oneshot;
use gpui_kit::*;

/// What to choose: the panel's title, its line saying what's allowed, its button, and the ends
/// of the names it takes (lower case).
#[derive(Clone, Copy)]
pub struct Choose {
    pub title: &'static str,
    pub message: &'static str,
    pub button: &'static str,
    pub suffixes: &'static [&'static str],
}

/// The avatar formats Lightair loads (`lightair_engine::avatar`).
pub const AVATAR: Choose = Choose {
    title: "Choose an avatar",
    message: "VRM (.vrm, or .glb with VRM data) · Live2D (.model3.json) · Inochi2D (.inp, .inx) · Umamo (.uma)",
    button: "Choose",
    // (a plain glTF without VRM's extension doesn't load: lightair-avatar's vrm.rs)
    suffixes: &[".vrm", ".glb", ".model3.json", ".inp", ".inx", ".uma"],
};

/// What an avatar can wear: a picture or a video.
pub const ACCESSORY: Choose = Choose {
    title: "Choose an accessory",
    message: "A picture or a video: .png, .jpg, .gif, .webp, .heic, .mov, .mp4 or .webm",
    button: "Choose",
    suffixes: &[".png", ".jpg", ".jpeg", ".gif", ".webp", ".heic", ".mov", ".mp4", ".m4v", ".webm"],
};

impl Choose {
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    pub fn takes(&self, path: &std::path::Path) -> bool {
        let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
        self.suffixes.iter().any(|s| name.ends_with(s))
    }
}

/// Asks for one file of this kind: `None` when cancelled.
pub fn pick(choose: Choose, cx: &mut App) -> oneshot::Receiver<Option<PathBuf>> {
    let (tx, rx) = oneshot::channel();
    #[cfg(target_os = "macos")]
    mac::open(choose, tx);
    #[cfg(not(target_os = "macos"))]
    {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: false,
            prompt: Some(choose.button.into()),
        });
        cx.spawn(async move |_| {
            let path = match paths.await {
                Ok(Ok(Some(paths))) => paths.into_iter().next().filter(|p| choose.takes(p)),
                _ => None,
            };
            let _ = tx.send(path);
        })
        .detach();
    }
    #[cfg(target_os = "macos")]
    let _ = cx;
    rx
}

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::CStr;
    use std::path::PathBuf;
    use std::sync::Mutex;

    use block2::RcBlock;
    use futures_channel::oneshot;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Bool, ClassBuilder, Sel};
    use objc2::{msg_send, sel};
    use objc2_foundation::NSString;

    use super::Choose;

    /// The ends of the names the open panel takes now (one panel at a time).
    static SUFFIXES: Mutex<&'static [&'static str]> = Mutex::new(&[]);

    fn path_of(url: *mut AnyObject) -> Option<String> {
        if url.is_null() {
            return None;
        }
        let path: Option<Retained<NSString>> = unsafe { msg_send![url, path] };
        path.map(|p| p.to_string())
    }

    /// `panel:shouldEnableURL:`: folders (to go into them) and the files the choice takes.
    extern "C-unwind" fn should_enable(_: *mut AnyObject, _: Sel, _panel: *mut AnyObject, url: *mut AnyObject) -> Bool {
        if url.is_null() {
            return Bool::NO;
        }
        let folder: bool = unsafe { msg_send![url, hasDirectoryPath] };
        if folder {
            return Bool::YES;
        }
        let name = path_of(url).unwrap_or_default().to_lowercase();
        let suffixes = *SUFFIXES.lock().unwrap();
        Bool::new(suffixes.iter().any(|s| name.ends_with(s)))
    }

    fn filter_class() -> Option<&'static AnyClass> {
        const NAME: &CStr = c"LightairOpenPanelFilter";
        if let Some(c) = AnyClass::get(NAME) {
            return Some(c);
        }
        let mut b = ClassBuilder::new(NAME, AnyClass::get(c"NSObject")?)?;
        unsafe {
            b.add_method(
                sel!(panel:shouldEnableURL:),
                should_enable as extern "C-unwind" fn(*mut AnyObject, Sel, *mut AnyObject, *mut AnyObject) -> Bool,
            );
        }
        Some(b.register())
    }

    pub(super) fn open(choose: Choose, tx: oneshot::Sender<Option<PathBuf>>) {
        let Some(panel_class) = AnyClass::get(c"NSOpenPanel") else {
            let _ = tx.send(None);
            return;
        };
        *SUFFIXES.lock().unwrap() = choose.suffixes;
        unsafe {
            let panel: Retained<AnyObject> = msg_send![panel_class, openPanel];
            let _: () = msg_send![&*panel, setCanChooseFiles: true];
            let _: () = msg_send![&*panel, setCanChooseDirectories: false];
            let _: () = msg_send![&*panel, setAllowsMultipleSelection: false];
            let _: () = msg_send![&*panel, setResolvesAliases: true];
            let _: () = msg_send![&*panel, setTitle: &*NSString::from_str(choose.title)];
            let _: () = msg_send![&*panel, setMessage: &*NSString::from_str(choose.message)];
            let _: () = msg_send![&*panel, setPrompt: &*NSString::from_str(choose.button)];
            let filter: Option<Retained<AnyObject>> = filter_class().and_then(|c| msg_send![c, new]);
            if let Some(f) = &filter {
                let _: () = msg_send![&*panel, setDelegate: &**f];
            }
            let tx = std::cell::Cell::new(Some(tx));
            let keep = panel.clone();
            let block = RcBlock::new(move |response: isize| {
                // (the delegate is weak: kept alive until the panel answers)
                let _ = &filter;
                let path = (response == 1)
                    .then(|| {
                        let urls: Option<Retained<AnyObject>> = msg_send![&*keep, URLs];
                        let first: *mut AnyObject = urls.map_or(std::ptr::null_mut(), |u| msg_send![&*u, firstObject]);
                        path_of(first)
                    })
                    .flatten()
                    .map(PathBuf::from);
                let _: () = msg_send![&*keep, setDelegate: std::ptr::null::<AnyObject>()];
                if let Some(tx) = tx.take() {
                    let _ = tx.send(path);
                }
            });
            // a sheet on Lightair's window, like a document app's
            let app: *mut AnyObject = msg_send![AnyClass::get(c"NSApplication").unwrap(), sharedApplication];
            let window: *mut AnyObject = msg_send![app, keyWindow];
            if window.is_null() {
                let _: () = msg_send![&*panel, beginWithCompletionHandler: &*block];
            } else {
                let _: () = msg_send![&*panel, beginSheetModalForWindow: window, completionHandler: &*block];
            }
        }
    }
}
