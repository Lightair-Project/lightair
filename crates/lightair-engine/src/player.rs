//! A media file played on its own (the Library's player): a private media source, not in any
//! scene, its sound heard through the monitoring device. Show it with a preview
//! (`Preview::set_source`, `set_own_size`). Media calls are safe from any thread.

use std::path::Path;

use crate::media::{MediaState, MediaStatus};
use crate::obs::{Data, ObsSource};
use crate::sys;

pub struct Player {
    source: ObsSource,
}

// SAFETY: libobs sources are reference counted and their media calls take their own locks.
unsafe impl Send for Player {}
unsafe impl Sync for Player {}

impl Player {
    /// Opens `path`, playing from the start.
    pub fn open(path: &Path) -> Option<Player> {
        let mut settings = Data::new();
        settings
            .set_bool("is_local_file", true)
            .set_str("local_file", &path.to_string_lossy())
            .set_bool("looping", false)
            .set_bool("restart_on_activate", false)
            .set_bool("close_when_inactive", false)
            .set_bool("hw_decode", true);
        // MP4 and QuickTime files through Lightair's own source (decoded into textures, no copy)
        let local = serde_json::json!({"is_local_file": true, "local_file": path.to_string_lossy()});
        let kind = if crate::kinds::native_media(&local) { crate::kinds::NATIVE_MEDIA } else { "ffmpeg_source" };
        let source = ObsSource::create_private(kind, "Library player", Some(&settings))?;
        unsafe { sys::obs_source_set_monitoring_type(source.as_ptr(), sys::OBS_MONITORING_TYPE_MONITOR_ONLY) };
        Some(Player { source })
    }

    /// The source, for a preview (borrowed; a preview takes its own reference).
    pub fn source_ptr(&self) -> *mut sys::obs_source_t {
        self.source.as_ptr()
    }

    /// A new strong reference to the source (what `Preview::set_source` takes).
    pub fn source_ref(&self) -> *mut sys::obs_source_t {
        unsafe { sys::obs_source_get_ref(self.source.as_ptr()) }
    }

    /// The picture's size (0×0 until the file has opened, and for sound only).
    pub fn size(&self) -> (u32, u32) {
        let p = self.source.as_ptr();
        unsafe { (sys::obs_source_get_width(p), sys::obs_source_get_height(p)) }
    }

    pub fn status(&self) -> MediaStatus {
        let p = self.source.as_ptr();
        unsafe {
            MediaStatus {
                state: crate::media::state(sys::obs_source_media_get_state(p)),
                time_ms: sys::obs_source_media_get_time(p),
                duration_ms: sys::obs_source_media_get_duration(p),
            }
        }
    }

    /// Plays, or pauses; from the start again once it has ended.
    pub fn toggle(&self) {
        let p = self.source.as_ptr();
        match self.status().state {
            MediaState::Ended | MediaState::Stopped => unsafe { sys::obs_source_media_restart(p) },
            MediaState::Playing => unsafe { sys::obs_source_media_play_pause(p, true) },
            _ => unsafe { sys::obs_source_media_play_pause(p, false) },
        }
    }

    pub fn seek(&self, ms: i64) {
        unsafe { sys::obs_source_media_set_time(self.source.as_ptr(), ms.max(0)) };
    }
}
