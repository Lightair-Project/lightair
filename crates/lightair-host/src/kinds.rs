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

use lightair_model::kinds::{APP, APP_AUDIO, CAMERA, DESKTOP_AUDIO, DISPLAY, GAME, MIC, SCREEN, WINDOW};
use serde_json::{Value, json};

/// The libobs source type and settings for a source, or `None` when its kind is libobs's own.
pub fn resolve(kind: &str, settings: &Value) -> Option<(&'static str, Value)> {
    // the soundboard: a media source with no file until a pad plays one
    if kind == crate::soundboard::KIND {
        return Some(("ffmpeg_source", crate::soundboard::settings("", false)));
    }
    if kind == "ffmpeg_source" {
        // a playlist plays through OBS's media source (any file type, one after another)
        if !crate::playlists::files(settings).is_empty() {
            return Some(("ffmpeg_source", crate::playlists::media_settings(settings)));
        }
        if native_media(settings) {
            return Some((NATIVE_MEDIA, settings.clone()));
        }
        return windows_hardware_decoding(settings).map(|s| ("ffmpeg_source", s));
    }
    let mut out = settings.as_object().cloned().unwrap_or_default();
    let mut take = |from: &str| out.remove(from);
    let mode = take("mode").and_then(|m| m.as_i64()).unwrap_or(DISPLAY);
    let display = take("display");
    let window = take("window");
    let app = take("app");
    let cursor = take("cursor").unwrap_or(json!(true));
    let device = take("device");
    let height = take("height").and_then(|h| h.as_u64());
    let device_or_default =
        || device.clone().filter(|d| d.as_str().is_some_and(|s| !s.is_empty())).unwrap_or(json!("default"));
    let mut set = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            out.insert(k.into(), v);
        }
    };
    let id = if cfg!(windows) {
        match kind {
            SCREEN if mode == DISPLAY => {
                // none chosen (a new capture, a project from a Mac): the primary monitor
                let display = display
                    .filter(|d| d.as_str().is_some_and(|s| !s.is_empty() && s != "DUMMY"))
                    .or_else(|| crate::displays::main_display_uuid().map(Value::from));
                set("monitor_id", display);
                set("capture_cursor", Some(cursor));
                "monitor_capture"
            }
            SCREEN if mode == GAME => {
                set("capture_cursor", Some(cursor));
                let mode = if window.as_ref().is_some_and(|w| w.as_str().is_some_and(|s| !s.is_empty())) {
                    "window"
                } else {
                    "any_fullscreen"
                };
                set("capture_mode", Some(json!(mode)));
                set("window", window);
                "game_capture"
            }
            SCREEN => {
                set("cursor", Some(cursor));
                if mode == APP {
                    // the app's window, found again by its executable
                    set("window", app);
                    set("priority", Some(json!(2)));
                } else {
                    set("window", window);
                }
                "window_capture"
            }
            CAMERA => {
                let id = device.as_ref().and_then(|d| d.as_str()).map(str::to_owned);
                set("video_device_id", device);
                // nothing off air runs (macOS: crate::on_screen); OBS's DirectShow source has
                // this itself (display capture already stops while hidden on Windows)
                set("deactivate_when_not_showing", Some(json!(true)));
                // the camera's own format nearest the height asked for (DirectShow shows nothing
                // at all when asked for one the camera doesn't have); none: the camera's default
                match height.zip(id).and_then(|(h, id)| camera_resolution(&id, h)) {
                    Some(res) => {
                        set("res_type", Some(json!(1)));
                        set("resolution", Some(json!(res)));
                    }
                    None => set("res_type", Some(json!(0))),
                }
                "dshow_input"
            }
            MIC => {
                set("device_id", Some(device_or_default()));
                "wasapi_input_capture"
            }
            DESKTOP_AUDIO => {
                set("device_id", Some(device_or_default()));
                "wasapi_output_capture"
            }
            APP_AUDIO => {
                set("window", app);
                set("priority", Some(json!(2)));
                "wasapi_process_output_capture"
            }
            _ => return None,
        }
    } else {
        match kind {
            SCREEN => {
                // a game is a window here
                set("type", Some(json!(if mode == GAME { WINDOW } else { mode.clamp(DISPLAY, APP) })));
                let display = display
                    .filter(|d| d.as_str().is_some_and(|s| !s.is_empty()))
                    .or_else(|| crate::displays::main_display_uuid().map(Value::from));
                set("display_uuid", display);
                set("window", window);
                set("application", app);
                set("show_cursor", Some(cursor));
                // OBS's screen_capture, run only while shown (crate::on_screen)
                "lightair_screen_capture"
            }
            CAMERA => {
                set("device", device);
                if let Some(h) = height {
                    let preset = match h {
                        720 => "AVCaptureSessionPreset1280x720",
                        1080 => "AVCaptureSessionPreset1920x1080",
                        2160 => "AVCaptureSessionPreset3840x2160",
                        _ => "AVCaptureSessionPresetHigh",
                    };
                    set("use_preset", Some(json!(true)));
                    set("preset", Some(json!(preset)));
                }
                // OBS's macos-avcapture, run only while shown (crate::on_screen)
                "lightair_camera_capture"
            }
            MIC => {
                set("device_id", Some(device_or_default()));
                "coreaudio_input_capture"
            }
            // a loopback device (BlackHole and the like), or everything macOS plays
            DESKTOP_AUDIO => match device.filter(|d| d.as_str().is_some_and(|s| !s.is_empty() && s != "default")) {
                Some(d) => {
                    set("device_id", Some(d));
                    "coreaudio_output_capture"
                }
                // Core Audio's process taps, not ScreenCaptureKit (crate::system_audio)
                None => {
                    set("type", Some(json!(0)));
                    "lightair_system_audio"
                }
            },
            APP_AUDIO => {
                set("type", Some(json!(1)));
                set("application", app);
                "lightair_system_audio"
            }
            _ => return None,
        }
    };
    Some((id, Value::Object(out)))
}

/// The resolution ("1280x720") of camera `device` that best gives a height (Windows: read by
/// libobs's DirectShow source, which installs this; `None` without it).
static CAMERA_RESOLUTION: std::sync::OnceLock<fn(&str, u64) -> Option<String>> = std::sync::OnceLock::new();

/// Installs how a camera's resolution is chosen (once).
pub fn set_camera_resolution(f: fn(&str, u64) -> Option<String>) {
    let _ = CAMERA_RESOLUTION.set(f);
}

fn camera_resolution(device: &str, height: u64) -> Option<String> {
    CAMERA_RESOLUTION.get().and_then(|f| f(device, height))
}

/// The cameras and capture cards: (name, the `device` setting: AVFoundation's unique id on
/// macOS, Media Foundation's symbolic link on Windows).
pub fn cameras() -> Vec<(String, String)> {
    #[cfg(any(target_os = "macos", windows))]
    {
        lightair_capture::camera::cameras().into_iter().map(|c| (c.name, c.id)).collect()
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    Vec::new()
}

/// Lightair's own media source (macOS, [`crate::media_source`]): video decoded straight into
/// textures.
pub const NATIVE_MEDIA: &str = "lightair_media";

/// Whether a media source (`ffmpeg_source`'s settings) plays with [`NATIVE_MEDIA`]: on macOS,
/// a local file AVFoundation plays (MP4, QuickTime), at normal speed. Anything else (other
/// containers, streams, a changed speed) stays with OBS's media source; `LIGHTAIR_MEDIA=ffmpeg`
/// sends everything there (for comparison).
pub fn native_media(settings: &Value) -> bool {
    if !cfg!(target_os = "macos") || std::env::var("LIGHTAIR_MEDIA").is_ok_and(|v| v == "ffmpeg") {
        return false;
    }
    let local = settings.get("is_local_file").and_then(Value::as_bool) != Some(false);
    let speed = settings.get("speed_percent").and_then(Value::as_i64).unwrap_or(100);
    let file = settings.get("local_file").and_then(Value::as_str).unwrap_or("");
    let ext = std::path::Path::new(file).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    local && speed == 100 && matches!(ext.as_deref(), Some("mp4" | "m4v" | "mov"))
}

/// Windows: media files decode on the GPU's video decoder (D3D11VA), unless the source says
/// otherwise. OBS leaves this off; on a PC it halves the app's CPU with two videos playing (a
/// 1080p60 and a 720p30 file: 3.0 → 1.6 G cycles/s on an i5-9300H, docs/benchmarks), where on
/// Apple Silicon software decoding is cheap and the copy back cost more (it stays off there).
/// FFmpeg falls back to software by itself where the decoder can't. `LIGHTAIR_MEDIA_DECODE=software`
/// turns it off (for comparison).
fn windows_hardware_decoding(settings: &Value) -> Option<Value> {
    if !cfg!(windows)
        || settings.get("hw_decode").is_some()
        || std::env::var("LIGHTAIR_MEDIA_DECODE").is_ok_and(|v| v == "software")
    {
        return None;
    }
    let mut s = settings.as_object().cloned().unwrap_or_default();
    s.insert("hw_decode".into(), json!(true));
    Some(Value::Object(s))
}

/// The libobs source type a source plays as (its own kind when that's libobs's).
pub fn libobs_kind<'a>(kind: &'a str, settings: &Value) -> &'a str {
    resolve(kind, settings).map_or(kind, |(id, _)| id)
}

/// The libobs setting holding a portable one (`key`), for listing its choices.
pub fn libobs_key(kind: &str, key: &str) -> Option<&'static str> {
    let win = cfg!(windows);
    Some(match (kind, key) {
        (SCREEN, "display") => {
            if win {
                "monitor_id"
            } else {
                "display_uuid"
            }
        }
        (SCREEN, "window") => "window",
        (SCREEN, "app") | (APP_AUDIO, "app") => {
            if win {
                "window"
            } else {
                "application"
            }
        }
        (CAMERA, "device") => {
            if win {
                "video_device_id"
            } else {
                "device"
            }
        }
        (MIC | DESKTOP_AUDIO, "device") => "device_id",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn portable_kinds_play_as_macos_sources() {
        let (id, s) = resolve(SCREEN, &json!({"mode": 1, "window": 7, "cursor": false, "hide_obs": true})).unwrap();
        assert_eq!(id, "lightair_screen_capture");
        assert_eq!(
            (&s["type"], &s["window"], &s["show_cursor"], &s["hide_obs"]),
            (&json!(1), &json!(7), &json!(false), &json!(true))
        );
        assert!(s.get("mode").is_none() && s.get("cursor").is_none());

        assert_eq!(resolve(MIC, &json!({})).unwrap(), ("coreaudio_input_capture", json!({"device_id": "default"})));
        assert_eq!(resolve(DESKTOP_AUDIO, &json!({})).unwrap(), ("lightair_system_audio", json!({"type": 0})));
        assert_eq!(
            resolve(DESKTOP_AUDIO, &json!({"device": "BlackHole2ch_UID"})).unwrap(),
            ("coreaudio_output_capture", json!({"device_id": "BlackHole2ch_UID"}))
        );
        assert_eq!(
            resolve(APP_AUDIO, &json!({"app": "com.spotify.client"})).unwrap(),
            ("lightair_system_audio", json!({"type": 1, "application": "com.spotify.client"}))
        );
        assert_eq!(
            resolve(CAMERA, &json!({"device": "x", "height": 720})).unwrap(),
            (
                "lightair_camera_capture",
                json!({"device": "x", "use_preset": true, "preset": "AVCaptureSessionPreset1280x720"})
            )
        );
        assert_eq!(libobs_key(SCREEN, "display"), Some("display_uuid"));
        assert!(resolve("color_source_v3", &json!({})).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn portable_kinds_play_as_windows_sources() {
        assert_eq!(resolve(SCREEN, &json!({"mode": 0})).unwrap().0, "monitor_capture");
        assert_eq!(resolve(SCREEN, &json!({"mode": 3})).unwrap().1["capture_mode"], "any_fullscreen");
        let (id, s) = resolve(SCREEN, &json!({"mode": 2, "app": "a:b:c.exe"})).unwrap();
        assert_eq!((id, &s["window"], &s["priority"]), ("window_capture", &json!("a:b:c.exe"), &json!(2)));
        assert_eq!(resolve(MIC, &json!({})).unwrap(), ("wasapi_input_capture", json!({"device_id": "default"})));
        assert_eq!(resolve(APP_AUDIO, &json!({})).unwrap().0, "wasapi_process_output_capture");
        assert_eq!(libobs_key(CAMERA, "device"), Some("video_device_id"));
    }
}
