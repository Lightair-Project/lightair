//! A Studio take's video (docs/plans/screen-studio.md, 4.2): each screen and camera in the
//! program recorded to a file of its own, at its own size, as fragmented MP4 (a take that stops
//! abruptly keeps what was written). On macOS: each source's frames as the
//! compositor captures them (`crate::bridge::publish_capture`) go to a recording of Lightair's
//! own, keyed by a feed of the take's.
//!
//! Screens are recorded as captured (no effects); so are cameras for now (their effects aren't
//! in the file). Sound goes to the take's multitrack files (the mixer's), started beside this.

use std::path::{Path, PathBuf};

use lightair_model::{Id, Source};

/// One recorded file.
#[derive(Debug, Clone)]
pub struct Track {
    /// "Screen", "Camera", "Camera 2"…
    pub name: String,
    /// A screen (else a camera): what it is, apart from its name.
    pub screen: bool,
    pub source: Id,
    pub file: PathBuf,
    pub width: u32,
    pub height: u32,
}

/// A take's per-source recordings, let go once they've all stopped.
#[derive(Debug, Default)]
pub struct StudioTake {
    pub tracks: Vec<Track>,
    /// Each track's feed (its recording's key).
    keys: Vec<Id>,
}

#[cfg(target_os = "macos")]
mod feeds {
    use std::collections::HashMap;

    use lightair_model::Id;
    use parking_lot::Mutex;

    /// The take's sources and the feeds their frames go to.
    pub(super) static FEEDS: Mutex<Option<HashMap<Id, Id>>> = Mutex::new(None);
}

/// A source's newest captured frame (the compositor's capture): to its take's recording, when a
/// take records it. Any thread.
#[cfg(target_os = "macos")]
pub fn frame(source: Id, buffer: &crate::buffers::Buffer) {
    let key = feeds::FEEDS.lock().as_ref().and_then(|f| f.get(&source).copied());
    if let Some(key) = key {
        crate::own_outputs::frame(key, buffer);
    }
}

#[cfg(target_os = "macos")]
fn even(v: u32) -> u32 {
    v.max(2) & !1
}

impl StudioTake {
    /// Starts recording `sources` (each with its id) into `folder`.
    pub fn start(
        sources: Vec<(Source, Id)>,
        folder: &Path,
        fps: (u32, u32),
        outputs: &mut crate::outputs_host::Outputs,
        encoders: &[String],
    ) -> Result<StudioTake, String> {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (sources, folder, fps, outputs, encoders);
            Err("Studio takes are recorded on macOS only for now.".into())
        }
        #[cfg(target_os = "macos")]
        {
            use objc2_core_video::{CVPixelBufferGetHeight, CVPixelBufferGetWidth};

            use crate::outputs::{Container, RecordingConfig, default_encoders};

            std::fs::create_dir_all(folder).map_err(|e| format!("Can't create the take's folder: {e}"))?;
            let mut take = StudioTake::default();
            let (mut screens, mut cameras) = (0, 0);
            for (source, id) in sources {
                let screen = source.kind == lightair_model::kinds::SCREEN;
                // its size as captured now
                let (w, h) = crate::bridge::captured(id)
                    .map(|b| (CVPixelBufferGetWidth(&b) as u32, CVPixelBufferGetHeight(&b) as u32))
                    .filter(|(w, h)| *w > 0 && *h > 0)
                    .map_or((1920, 1080), |(w, h)| (even(w), even(h)));
                let key = Id::new();
                crate::program::set_feed_shape(key, Some((w, h, fps.0.max(1), fps.1.max(1))));
                let name = if screen {
                    screens += 1;
                    if screens == 1 { "Screen".to_string() } else { format!("Screen {screens}") }
                } else {
                    cameras += 1;
                    if cameras == 1 { "Camera".to_string() } else { format!("Camera {cameras}") }
                };
                let file = folder.join(format!("{}.mp4", name.to_lowercase().replace(' ', "-")));
                // a generous bitrate: the Editor re-encodes from these
                let kbps =
                    ((w as u64 * h as u64 * fps.0 as u64 / fps.1.max(1) as u64) / 40_000).clamp(6_000, 80_000) as u32;
                let mut enc = default_encoders(encoders, kbps, false);
                enc.keyint_sec = 1;
                let cfg = RecordingConfig {
                    canvas: key,
                    path: file.clone(),
                    label: None,
                    split_minutes: None,
                    container: Container::HybridMp4,
                    encoders: enc,
                    extra_mixes: Vec::new(),
                };
                feeds::FEEDS.lock().get_or_insert_with(Default::default).insert(id, key);
                if let Err(e) = outputs.start_recording(&cfg, Some(key)) {
                    take.let_go();
                    return Err(e);
                }
                take.keys.push(key);
                take.tracks.push(Track { name, screen, source: id, file, width: w, height: h });
            }
            if take.tracks.is_empty() {
                return Err("The program has no screen or camera to record.".into());
            }
            Ok(take)
        }
    }

    /// Whether every file has stopped (the take can be let go).
    pub fn finished(&self) -> bool {
        use crate::outputs::{OutputKey, runtime};
        self.keys.iter().all(|k| !runtime().get(OutputKey::Recording(*k)).state.is_running())
    }

    /// The feeds and their shapes let go.
    fn let_go(&mut self) {
        #[cfg(target_os = "macos")]
        {
            let sources: Vec<Id> = self.tracks.iter().map(|t| t.source).collect();
            if let Some(f) = feeds::FEEDS.lock().as_mut() {
                f.retain(|source, key| !sources.contains(source) && !self.keys.contains(key));
            }
        }
        for key in self.keys.drain(..) {
            crate::program::set_feed_shape(key, None);
        }
    }
}

impl Drop for StudioTake {
    fn drop(&mut self) {
        self.let_go();
    }
}
