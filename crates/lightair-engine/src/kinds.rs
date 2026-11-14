//! Lightair's portable input kinds (`lightair_core::kinds`) as this platform's libobs sources:
//! which source type plays each, and its settings under libobs's names.
//!
//! | Kind | macOS | Windows |
//! |---|---|---|
//! | screen: display | `screen_capture` (type 0) | `monitor_capture` |
//! | screen: window | `screen_capture` (type 1) | `window_capture` |
//! | screen: app | `screen_capture` (type 2) | `window_capture` (by executable) |
//! | camera | `macos-avcapture` | `dshow_input` |
//! | mic | `coreaudio_input_capture` | `wasapi_input_capture` |
//! | desktop audio | `sck_audio_capture` (type 0); a loopback device: `coreaudio_output_capture` | `wasapi_output_capture` |
//! | app audio | `sck_audio_capture` (type 1) | `wasapi_process_output_capture` |
//!
//! Settings that aren't Lightair's own (a camera's preset, say) pass through as they are.

use lightair_core::kinds::{APP, APP_AUDIO, CAMERA, DESKTOP_AUDIO, DISPLAY, MIC, SCREEN};
use serde_json::{Value, json};

/// The libobs source type and settings for an input, or `None` when its kind is libobs's own.
pub fn resolve(kind: &str, settings: &Value) -> Option<(&'static str, Value)> {
    let mut out = settings.as_object().cloned().unwrap_or_default();
    let mut take = |from: &str| out.remove(from);
    let mode = take("mode").and_then(|m| m.as_i64()).unwrap_or(DISPLAY);
    let display = take("display");
    let window = take("window");
    let app = take("app");
    let cursor = take("cursor").unwrap_or(json!(true));
    let device = take("device");
    let height = take("height").and_then(|h| h.as_u64());
    let device_or_default = || device.clone().filter(|d| d.as_str().is_some_and(|s| !s.is_empty())).unwrap_or(json!("default"));
    let mut set = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            out.insert(k.into(), v);
        }
    };
    let id = if cfg!(windows) {
        match kind {
            SCREEN if mode == DISPLAY => {
                set("monitor_id", display);
                set("capture_cursor", Some(cursor));
                "monitor_capture"
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
                set("video_device_id", device);
                if let Some(h) = height {
                    // custom resolution, 16:9
                    set("res_type", Some(json!(1)));
                    set("resolution", Some(json!(format!("{}x{h}", h * 16 / 9))));
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
                set("type", Some(json!(mode.clamp(DISPLAY, APP))));
                let display = display
                    .filter(|d| d.as_str().is_some_and(|s| !s.is_empty()))
                    .or_else(|| crate::displays::main_display_uuid().map(Value::from));
                set("display_uuid", display);
                set("window", window);
                set("application", app);
                set("show_cursor", Some(cursor));
                "screen_capture"
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
                "macos-avcapture"
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
                None => {
                    set("type", Some(json!(0)));
                    "sck_audio_capture"
                }
            },
            APP_AUDIO => {
                set("type", Some(json!(1)));
                set("application", app);
                "sck_audio_capture"
            }
            _ => return None,
        }
    };
    Some((id, Value::Object(out)))
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

/// The libobs source type an input plays as (its own kind when that's libobs's).
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
        assert_eq!(id, "screen_capture");
        assert_eq!((&s["type"], &s["window"], &s["show_cursor"], &s["hide_obs"]), (&json!(1), &json!(7), &json!(false), &json!(true)));
        assert!(s.get("mode").is_none() && s.get("cursor").is_none());

        assert_eq!(resolve(MIC, &json!({})).unwrap(), ("coreaudio_input_capture", json!({"device_id": "default"})));
        assert_eq!(resolve(DESKTOP_AUDIO, &json!({})).unwrap(), ("sck_audio_capture", json!({"type": 0})));
        assert_eq!(
            resolve(DESKTOP_AUDIO, &json!({"device": "BlackHole2ch_UID"})).unwrap(),
            ("coreaudio_output_capture", json!({"device_id": "BlackHole2ch_UID"}))
        );
        assert_eq!(
            resolve(APP_AUDIO, &json!({"app": "com.spotify.client"})).unwrap(),
            ("sck_audio_capture", json!({"type": 1, "application": "com.spotify.client"}))
        );
        assert_eq!(
            resolve(CAMERA, &json!({"device": "x", "height": 720})).unwrap(),
            ("macos-avcapture", json!({"device": "x", "use_preset": true, "preset": "AVCaptureSessionPreset1280x720"}))
        );
        assert_eq!(libobs_key(SCREEN, "display"), Some("display_uuid"));
        assert!(resolve("color_source_v3", &json!({})).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn portable_kinds_play_as_windows_sources() {
        assert_eq!(resolve(SCREEN, &json!({"mode": 0})).unwrap().0, "monitor_capture");
        let (id, s) = resolve(SCREEN, &json!({"mode": 2, "app": "a:b:c.exe"})).unwrap();
        assert_eq!((id, &s["window"], &s["priority"]), ("window_capture", &json!("a:b:c.exe"), &json!(2)));
        assert_eq!(resolve(MIC, &json!({})).unwrap(), ("wasapi_input_capture", json!({"device_id": "default"})));
        assert_eq!(resolve(APP_AUDIO, &json!({})).unwrap().0, "wasapi_process_output_capture");
        assert_eq!(libobs_key(CAMERA, "device"), Some("video_device_id"));
    }
}
