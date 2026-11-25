//! The co-host's face on libobs's side (the face itself is `lightair_host::face_drive`,
//! re-exported here): an avatar source's face by its libobs source.

use std::ffi::CStr;
#[cfg(test)]
use std::sync::Mutex;
#[cfg(test)]
use std::time::Duration;
use std::time::Instant;

use lightair_avatar::Face;

pub use lightair_host::face_drive::*;

use crate::sys;

/// The face for avatar `source` this frame, from what its camera sees (`seen`), and an expression
/// (name, weight) that overrides the avatar's own while the co-host has a mood. The target is
/// the source's Id (its libobs name), so renaming the object doesn't lose it. Costs one atomic
/// load when no avatar is the co-host's.
pub(crate) fn merge(source: *mut sys::obs_source_t, seen: Option<Face>) -> (Option<Face>, Option<(&'static str, f32)>) {
    if source.is_null() || !lightair_host::face_drive::ON.load(std::sync::atomic::Ordering::Relaxed) {
        return (seen, None);
    }
    let name = unsafe { sys::obs_source_get_name(source) };
    if name.is_null() {
        return (seen, None);
    }
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    merge_at(&name, Instant::now(), seen)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The co-host's face is process-wide: these tests take turns.
    static SERIAL: Mutex<()> = Mutex::new(());

    fn camera() -> Face {
        Face { mouth_open: 0.2, mouth_wide: 0.1, ..Face::default() }
    }

    /// With no avatar chosen as the co-host's, every avatar's face passes through untouched (the
    /// check is made before the source is looked at).
    #[test]
    fn with_no_co_host_every_face_passes_through() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        set_target(None);
        push_voice(1.0, 0.5);
        let source = std::ptr::NonNull::<sys::obs_source_t>::dangling().as_ptr();
        assert_eq!(merge(source, Some(camera())), (Some(camera()), None));
    }

    /// Another avatar than the co-host's keeps its own face while the co-host speaks.
    #[test]
    fn another_avatars_face_passes_through() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        set_target(Some("cohost".into()));
        push_voice(1.0, 0.5);
        let soon = Instant::now() + VOICE_DELAY + Duration::from_millis(5);
        assert_eq!(merge_at("someone else", soon, Some(camera())), (Some(camera()), None));
    }

    /// On the co-host's avatar with a camera face, the camera keeps head and eyes while the voice
    /// drives the mouth; when the voice stops, the mouth is the camera's again.
    #[test]
    fn the_co_hosts_mouth_follows_the_voice_then_goes_back_to_the_camera() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        set_target(Some("cohost".into()));
        push_voice(0.9, -0.4);
        let heard = Instant::now() + VOICE_DELAY + Duration::from_millis(5);
        let (face, _) = merge_at("cohost", heard, Some(camera()));
        let face = face.unwrap();
        assert_eq!((face.mouth_open, face.mouth_wide), (0.9, -0.4), "the mouth from the voice");
        assert_eq!(
            (face.head, face.gaze, face.eye_open),
            (camera().head, camera().gaze, camera().eye_open),
            "head and eyes the camera's"
        );

        let quiet = heard + Duration::from_millis(500);
        let (face, _) = merge_at("cohost", quiet, Some(camera()));
        assert_eq!(face, Some(camera()), "the voice stopped: the camera's mouth");
    }

    /// With no camera face, the co-host's avatar gets the AI face whole, mouth from the voice.
    #[test]
    fn without_a_camera_the_co_host_gets_the_whole_ai_face() {
        let _turn = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        set_target(Some("cohost".into()));
        push_voice(0.7, 0.2);
        let heard = Instant::now() + VOICE_DELAY + Duration::from_millis(5);
        let face = merge_at("cohost", heard, None).0.expect("a face");
        assert_eq!((face.mouth_open, face.mouth_wide), (0.7, 0.2));
    }
}
