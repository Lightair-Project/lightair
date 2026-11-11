//! Media playback control for media inputs (video files): runtime state, not document state
//! (docs/07-scenes-layers-canvas.md, "Media layer"). Looping and the file live in settings.

use libobs_sys as sys;
use lightair_core::Id;

use crate::reconcile::Reconciler;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaState {
    None,
    Playing,
    Opening,
    Buffering,
    Paused,
    Stopped,
    Ended,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaStatus {
    pub state: MediaState,
    pub time_ms: i64,
    pub duration_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MediaAction {
    Play,
    Pause,
    Restart,
    Stop,
    /// Seek to a position in milliseconds.
    Seek(i64),
}

fn state(s: sys::obs_media_state) -> MediaState {
    match s {
        sys::OBS_MEDIA_STATE_PLAYING => MediaState::Playing,
        sys::OBS_MEDIA_STATE_OPENING => MediaState::Opening,
        sys::OBS_MEDIA_STATE_BUFFERING => MediaState::Buffering,
        sys::OBS_MEDIA_STATE_PAUSED => MediaState::Paused,
        sys::OBS_MEDIA_STATE_STOPPED => MediaState::Stopped,
        sys::OBS_MEDIA_STATE_ENDED => MediaState::Ended,
        sys::OBS_MEDIA_STATE_ERROR => MediaState::Error,
        _ => MediaState::None,
    }
}

impl Reconciler {
    /// Playback status of a media input, or `None` if the input isn't a media source.
    pub fn media_status(&self, input: Id) -> Option<MediaStatus> {
        let src = self.input_ptr(input)?;
        let flags = unsafe { sys::obs_source_get_output_flags(src) };
        if flags & sys::OBS_SOURCE_CONTROLLABLE_MEDIA == 0 {
            return None;
        }
        unsafe {
            Some(MediaStatus {
                state: state(sys::obs_source_media_get_state(src)),
                time_ms: sys::obs_source_media_get_time(src),
                duration_ms: sys::obs_source_media_get_duration(src),
            })
        }
    }

    pub fn media_action(&self, input: Id, action: MediaAction) {
        let Some(src) = self.input_ptr(input) else { return };
        unsafe {
            match action {
                MediaAction::Play => sys::obs_source_media_play_pause(src, false),
                MediaAction::Pause => sys::obs_source_media_play_pause(src, true),
                MediaAction::Restart => sys::obs_source_media_restart(src),
                MediaAction::Stop => sys::obs_source_media_stop(src),
                MediaAction::Seek(ms) => sys::obs_source_media_set_time(src, ms.max(0)),
            }
        }
    }
}
