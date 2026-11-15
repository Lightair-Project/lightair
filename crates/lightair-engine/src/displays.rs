//! Display identifiers as OBS's capture sources expect them, and the displays there are (each
//! with a capture of its own, for a picker's live thumbnails).

/// UUID of the main display, as used by `screen_capture`'s `display_uuid` setting.
#[cfg(target_os = "macos")]
pub fn main_display_uuid() -> Option<String> {
    use std::ffi::{c_char, c_void};

    type CFTypeRef = *const c_void;
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGMainDisplayID() -> u32;
    }
    #[link(name = "ColorSync", kind = "framework")]
    unsafe extern "C" {
        fn CGDisplayCreateUUIDFromDisplayID(display: u32) -> CFTypeRef;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFUUIDCreateString(alloc: CFTypeRef, uuid: CFTypeRef) -> CFTypeRef;
        fn CFStringGetCString(s: CFTypeRef, buf: *mut c_char, size: isize, encoding: u32) -> bool;
        fn CFRelease(cf: CFTypeRef);
    }
    const UTF8: u32 = 0x0800_0100;

    unsafe {
        let uuid = CGDisplayCreateUUIDFromDisplayID(CGMainDisplayID());
        if uuid.is_null() {
            return None;
        }
        let text = CFUUIDCreateString(std::ptr::null(), uuid);
        CFRelease(uuid);
        if text.is_null() {
            return None;
        }
        let mut buf = [0 as c_char; 64];
        let ok = CFStringGetCString(text, buf.as_mut_ptr(), buf.len() as isize, UTF8);
        CFRelease(text);
        ok.then(|| std::ffi::CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned())
    }
}

#[cfg(not(target_os = "macos"))]
pub fn main_display_uuid() -> Option<String> {
    None
}

/// A display there is, as the display capture source lists it.
pub struct Display {
    /// "Built-in Retina Display".
    pub name: String,
    /// Its size as the capture source reports it ("1512x982"), parsed.
    pub size: Option<(u32, u32)>,
    /// The portable `display` setting that captures it.
    pub value: serde_json::Value,
    /// The main display (the one with the menu bar; Windows's primary monitor).
    pub main: bool,
    /// A private capture of it, for a thumbnail (dropped on the engine thread).
    pub capture: Option<crate::obs::ObsSource>,
}

/// "Built-in Retina Display: 1512x982 @ 0,0" → ("Built-in Retina Display", 1512×982, main?).
fn parse_label(label: &str) -> (String, Option<(u32, u32)>, bool) {
    let (name, rest) = label.split_once(": ").unwrap_or((label, ""));
    let size = rest.split_whitespace().next().and_then(|wh| wh.split_once('x')).and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)));
    (name.trim().to_string(), size, rest.contains("Primary"))
}

/// The displays, each with a capture of it (`captures`). Engine thread.
pub fn list(captures: bool) -> Vec<Display> {
    use lightair_core::kinds::{DISPLAY, SCREEN};
    let settings = |display: Option<&serde_json::Value>| {
        let mut s = serde_json::json!({"mode": DISPLAY, "cursor": true});
        if let Some(d) = display {
            s["display"] = d.clone();
        }
        crate::kinds::resolve(SCREEN, &s)
    };
    let (Some((kind, first)), Some(key)) = (settings(None), crate::kinds::libobs_key(SCREEN, "display")) else {
        return vec![];
    };
    let Some(lister) = crate::obs::ObsSource::create_private(kind, "Lightair displays", Some(&crate::obs::Data::from_json(&first))) else {
        return vec![];
    };
    let main = main_display_uuid();
    crate::obs::source_list_items(lister.as_ptr(), key)
        .into_iter()
        .map(|(label, value)| {
            let (name, size, primary) = parse_label(&label);
            let capture = captures
                .then(|| settings(Some(&value)))
                .flatten()
                .and_then(|(kind, s)| crate::obs::ObsSource::create_private(kind, &name, Some(&crate::obs::Data::from_json(&s))));
            let is_main = primary || (main.is_some() && value.as_str() == main.as_deref());
            Display { name, size, value, main: is_main, capture }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn display_labels_read_as_name_and_size() {
        assert_eq!(
            super::parse_label("Built-in Retina Display: 1512x982 @ 0,0"),
            ("Built-in Retina Display".into(), Some((1512, 982)), false)
        );
        assert_eq!(super::parse_label("DELL U2720Q: 3840x2160 @ 0,0 (Primary Monitor)"), ("DELL U2720Q".into(), Some((3840, 2160)), true));
        assert_eq!(super::parse_label("Screen"), ("Screen".into(), None, false));
    }
}
