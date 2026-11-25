//! Lightair's portable source kinds (`lightair_model::kinds`) as this platform's libobs sources:
//! which source type plays each, and its settings under libobs's names.
//!
//! | Kind | macOS | Windows |
//! |---|---|---|
//! | screen: display | `screen_capture` (type 0)* | `monitor_capture` |
//! | screen: window | `screen_capture` (type 1)* | `window_capture` |
//! | screen: app | `screen_capture` (type 2)* | `window_capture` (by executable) |
//! | screen: game | `screen_capture` (type 1)* | `game_capture` (a window, or any full-screen game) |
//! | camera | `macos-avcapture`* | `dshow_source` |
//! | mic | `coreaudio_source_capture` | `wasapi_source_capture` |
//! | desktop audio | `lightair_system_audio` (type 0, a Core Audio tap); a loopback device: `coreaudio_output_capture` | `wasapi_output_capture` |
//! | app audio | `lightair_system_audio` (type 1) | `wasapi_process_output_capture` |
//! | media file (`ffmpeg_source`) | MP4/QuickTime: `lightair_media`; others `ffmpeg_source` | `ffmpeg_source` |
//!
//! \* through `lightair_screen_capture` / `lightair_camera_capture`, which run them only while
//! something shows them (`crate::on_screen`).
//!
//! Settings that aren't Lightair's own (a camera's preset, say) pass through as they are.

pub use lightair_host::kinds::*;
#[cfg(windows)]
use serde_json::json;

/// Windows: the resolution ("1280x720") of the camera `device` (DirectShow's device id) that
/// best gives `height`: the tallest it has up to that height, 16:9 first; `None` when its formats
/// can't be read. DirectShow's own properties list them without opening the camera; remembered
/// per camera and height (sources are resolved often).
#[cfg(windows)]
fn camera_resolution(device: &str, height: u64) -> Option<String> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};
    /// (device, height) → the resolution chosen
    type Seen = HashMap<(String, u64), Option<String>>;
    static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    if let Some(r) = seen.lock().ok()?.get(&(device.to_string(), height)) {
        return r.clone();
    }
    let sizes: Vec<(u64, u64)> = crate::obs::property_items_with("dshow_input", "resolution", &json!({"video_device_id": device}))
        .into_iter()
        .filter_map(|(_, v)| {
            let (w, h) = v.split_once('x')?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        })
        .collect();
    let wide = |&(w, h): &(u64, u64)| w * 9 == h * 16;
    let fits: Vec<(u64, u64)> = sizes.iter().copied().filter(|&(_, h)| h <= height).collect();
    let best = fits
        .iter()
        .copied()
        .max_by_key(|s| (s.1, wide(s), s.0))
        // nothing that small: the smallest it has
        .or_else(|| sizes.iter().copied().min_by_key(|s| (s.1, !wide(s))));
    let found = best.map(|(w, h)| format!("{w}x{h}"));
    if let Ok(mut s) = seen.lock() {
        s.insert((device.to_string(), height), found.clone());
    }
    found
}

/// Has `resolve` choose cameras' resolutions through libobs's DirectShow source (Windows).
pub(crate) fn install() {
    #[cfg(windows)]
    lightair_host::kinds::set_camera_resolution(camera_resolution);
}

/// The cameras and capture cards: (name, the `device` setting). Call on the engine thread.
pub fn cameras() -> Vec<(String, String)> {
    if cfg!(windows) {
        crate::obs::property_items("dshow_input", "video_device_id")
    } else {
        // OBS 33's camera source can't list devices without an instance; the older source
        // lists the same AVFoundation device ids
        crate::obs::property_items("av_capture_input_v2", "device")
    }
}

