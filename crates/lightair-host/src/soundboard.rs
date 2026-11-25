//! The soundboard (docs/13, "Built-in replacements"): one source of kind [`KIND`] with a mixer
//! track of its own plays the pads' sounds, one at a time, into the mix (so the stream and
//! recordings hear them, and its fader and effects apply). It's in no scene: an output channel
//! of its own keeps it running. A pad sets its file and starts it again.

use serde_json::{Value, json};

pub const KIND: &str = "lightair_soundboard";

/// The media source's settings for `file` (empty: none).
pub fn settings(file: &str, looped: bool) -> Value {
    json!({
        "is_local_file": true,
        "local_file": file,
        "looping": looped,
        "restart_on_activate": false,
        "close_when_inactive": false,
        "clear_on_media_end": true,
        "hw_decode": false,
    })
}
