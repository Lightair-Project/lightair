//! Media playback status and control for media sources (video files, the soundboard): runtime
//! state, not document state. Looping and the file live in settings.

/// A media state code, numbered as OBS's media states are (what the app and the remote API
/// read).
pub type MediaStateCode = u32;

/// The media states.
pub mod state {
    use super::MediaStateCode;
    pub const NONE: MediaStateCode = 0;
    pub const PLAYING: MediaStateCode = 1;
    pub const OPENING: MediaStateCode = 2;
    pub const BUFFERING: MediaStateCode = 3;
    pub const PAUSED: MediaStateCode = 4;
    pub const STOPPED: MediaStateCode = 5;
    pub const ENDED: MediaStateCode = 6;
    pub const ERROR: MediaStateCode = 7;
}

/// Where a media source's playback is.
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

impl MediaState {
    /// From a media state code ([`state`], numbered as OBS's are).
    pub fn from_code(code: u32) -> MediaState {
        match code {
            1 => MediaState::Playing,
            2 => MediaState::Opening,
            3 => MediaState::Buffering,
            4 => MediaState::Paused,
            5 => MediaState::Stopped,
            6 => MediaState::Ended,
            7 => MediaState::Error,
            _ => MediaState::None,
        }
    }
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
