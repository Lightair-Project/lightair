//! PiAir's face for an avatar (the AI co-host, piair/docs/design.md §5): the mouth from the voice
//! being played ([`crate::piair_voice`]), blinks, small idle head motion, a gaze target and a mood.
//! It fills the same `rig::Face` a camera does, so every avatar type takes it unchanged.
//!
//! [`merge`] runs in the avatar's tick: on the co-host's avatar, with no face from its camera the
//! AI face is used whole; with one, the camera keeps head and eyes and the AI drives the mouth
//! while it speaks. Everything else passes through untouched.

use std::collections::VecDeque;
use std::ffi::CStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use glam::{Quat, Vec2};
use lightair_avatar::Face;

use crate::sys;

/// How long the sound takes from the voice source to the speakers and the stream, about: the
/// mouth is read this far in the past so it moves with what's heard.
const VOICE_DELAY: Duration = Duration::from_millis(70);

/// What moves the co-host's mouth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouthSource {
    /// The voice vendor's timing data (mouth shapes in time with the words), and the sound where
    /// the vendor gives none.
    Data,
    /// The sound of the voice alone: its loudness opens the mouth, its brightness shapes it.
    Audio,
    /// The avatar's own face tracking (its camera): the voice never moves the mouth.
    Tracking,
}

static MOUTH: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// Sets what moves the co-host's mouth.
pub fn set_mouth_source(m: MouthSource) {
    MOUTH.store(m as u8, std::sync::atomic::Ordering::Relaxed);
}

/// What moves the co-host's mouth.
pub fn mouth_source() -> MouthSource {
    match MOUTH.load(std::sync::atomic::Ordering::Relaxed) {
        1 => MouthSource::Audio,
        2 => MouthSource::Tracking,
        _ => MouthSource::Data,
    }
}

/// Where the co-host looks.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Look {
    Camera,
    /// Towards the chat (down and to the side).
    Chat,
    /// Towards the streamer (to the side, level).
    Streamer,
    /// Wherever the pointer is on the main display.
    Pointer,
    /// A point: x right, y up, -1..1.
    At(Vec2),
}

struct State {
    /// The co-host's avatar (its source's libobs name, which is the model Id).
    target: Option<String>,
    /// The voice's mouth as it was played: when, open 0..1, wide -1..1.
    voice: VecDeque<(Instant, f32, f32)>,
    mood: Option<(String, Instant)>,
    look: (Look, Instant),
    started: Instant,
    next_blink: Instant,
    blink_at: Option<Instant>,
    saccade: (Vec2, Instant),
    /// A nod at the end of a sentence (when it started).
    nod: Option<Instant>,
    rng: u64,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);
/// Whether any avatar is the co-host's: checked first, without a lock, on every avatar's frame.
static ON: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn with<R>(f: impl FnOnce(&mut State) -> R) -> R {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    let s = g.get_or_insert_with(|| State {
        target: None,
        voice: VecDeque::new(),
        mood: None,
        look: (Look::Camera, now),
        started: now,
        next_blink: now + Duration::from_secs(2),
        blink_at: None,
        saccade: (Vec2::ZERO, now),
        nod: None,
        rng: 0x9E37_79B9_7F4A_7C15,
    });
    f(s)
}

fn rand(s: &mut State) -> f32 {
    // xorshift: good enough for blinks
    s.rng ^= s.rng << 13;
    s.rng ^= s.rng >> 7;
    s.rng ^= s.rng << 17;
    (s.rng >> 40) as f32 / (1u64 << 24) as f32
}

/// Which avatar is the co-host's (its source Id), or none.
pub fn set_target(id: Option<String>) {
    ON.store(id.is_some(), std::sync::atomic::Ordering::Relaxed);
    with(|s| s.target = id);
}

pub fn target() -> Option<String> {
    with(|s| s.target.clone())
}

/// The voice's mouth for the block just played.
pub fn push_voice(open: f32, wide: f32) {
    with(|s| {
        let now = Instant::now();
        s.voice.push_back((now, open, wide));
        while s.voice.front().is_some_and(|v| now.duration_since(v.0) > Duration::from_millis(600)) {
            s.voice.pop_front();
        }
    });
}

/// A sentence ended: a small nod.
pub fn nod() {
    with(|s| s.nod = Some(Instant::now()));
}

/// The co-host's mood for the next few seconds (`happy`, `sad`, `surprised`, `angry`, `thinking`,
/// `neutral`).
pub fn set_mood(mood: &str) {
    with(|s| s.mood = if mood == "neutral" { None } else { Some((mood.to_owned(), Instant::now())) });
}

/// Looks at a point: x right, y up, -1..1.
pub fn set_look_at(x: f32, y: f32) {
    set_look(Look::At(Vec2::new(x.clamp(-1.0, 1.0), y.clamp(-1.0, 1.0))));
}

pub fn set_look(look: Look) {
    with(|s| s.look = (look, Instant::now()));
}

/// The mouth `VOICE_DELAY` ago, and whether the voice was speaking then.
fn voice_now(s: &State, now: Instant) -> Option<(f32, f32)> {
    let at = now.checked_sub(VOICE_DELAY)?;
    let mut best: Option<(f32, f32)> = None;
    for &(t, o, w) in &s.voice {
        if t <= at {
            best = Some((o, w));
        } else {
            break;
        }
    }
    // the last block is older than a few blocks: the voice stopped
    let last = s.voice.back()?;
    if at.duration_since(last.0.min(at)) > Duration::from_millis(80) {
        return None;
    }
    best
}

/// The mood's brows, VRM expression and a head tilt.
fn mood_parts(mood: &str) -> ([f32; 2], Option<&'static str>, f32) {
    match mood {
        "happy" => ([0.3, 0.3], Some("happy"), 0.0),
        "surprised" => ([0.9, 0.9], Some("surprised"), 0.0),
        "sad" => ([-0.2, 0.4], Some("sad"), -4.0),
        "angry" => ([-0.8, -0.8], Some("angry"), 0.0),
        "thinking" => ([0.5, -0.2], Some("relaxed"), 6.0),
        _ => ([0.0, 0.0], None, 0.0),
    }
}

/// The face for avatar `source` this frame, from what its camera sees (`seen`), and an expression
/// (name, weight) that overrides the avatar's own while the co-host has a mood. The target is
/// the source's Id (its libobs name), so renaming the object doesn't lose it. Costs one atomic
/// load when no avatar is the co-host's.
pub(crate) fn merge(source: *mut sys::obs_source_t, seen: Option<Face>) -> (Option<Face>, Option<(&'static str, f32)>) {
    if source.is_null() || !ON.load(std::sync::atomic::Ordering::Relaxed) {
        return (seen, None);
    }
    let name = unsafe { sys::obs_source_get_name(source) };
    if name.is_null() {
        return (seen, None);
    }
    let name = unsafe { CStr::from_ptr(name) }.to_string_lossy();
    merge_at(&name, Instant::now(), seen)
}

/// [`merge`] for the avatar source named `name` (its Id) at `now`: the AI face, or `seen` as it is
/// when `name` isn't the co-host's avatar.
pub fn merge_at(name: &str, now: Instant, seen: Option<Face>) -> (Option<Face>, Option<(&'static str, f32)>) {
    let mut g = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(s) = g.as_mut() else { return (seen, None) };
    if s.target.as_deref() != Some(name) {
        return (seen, None);
    }
    let t = now.duration_since(s.started).as_secs_f32();
    let voice = voice_now(s, now);
    // a mood lasts 6 s
    if s.mood.as_ref().is_some_and(|m| now.duration_since(m.1) > Duration::from_secs(6)) {
        s.mood = None;
    }
    let (brow, expression, tilt) = s.mood.as_ref().map_or(([0.0; 2], None, 0.0), |m| mood_parts(&m.0));
    let expression = expression.map(|e| (e, 0.8));
    let tracking = mouth_source() == MouthSource::Tracking;
    // face tracking moves the mouth: the voice doesn't
    let voice = if tracking { None } else { voice };
    let (open, wide) = voice.unwrap_or((0.0, 0.0));

    if let Some(mut face) = seen {
        // the camera keeps the head and eyes; the AI speaks
        if voice.is_some() {
            face.mouth_open = open;
            face.mouth_wide = wide;
        }
        return (Some(face), expression);
    }

    // blinks: every 2–6 s, 150 ms
    if s.blink_at.is_none() && now >= s.next_blink {
        s.blink_at = Some(now);
        let wait = 2.0 + rand(s) * 4.0;
        s.next_blink = now + Duration::from_secs_f32(wait);
    }
    let eye = match s.blink_at {
        Some(b) => {
            let p = now.duration_since(b).as_secs_f32() / 0.15;
            if p >= 1.0 {
                s.blink_at = None;
                1.0
            } else {
                (1.0 - (p * std::f32::consts::PI).sin()).max(0.0)
            }
        }
        None => 1.0,
    };
    // gaze: the target, with small saccades every second or two
    if now >= s.saccade.1 {
        let jitter = Vec2::new(rand(s) - 0.5, rand(s) - 0.5) * 0.12;
        let wait = 0.8 + rand(s) * 1.6;
        s.saccade = (jitter, now + Duration::from_secs_f32(wait));
    }
    let look = match s.look.0 {
        Look::Camera => Vec2::ZERO,
        Look::Chat => Vec2::new(0.55, -0.35),
        Look::Streamer => Vec2::new(-0.6, 0.0),
        Look::At(p) => p,
        Look::Pointer => {
            let (px, py) = lightair_platform::input::pointer();
            let (dx, dy, dw, dh) = lightair_platform::input::main_display();
            if dw > 0.0 && dh > 0.0 {
                Vec2::new(((px - dx) / dw) * 2.0 - 1.0, 1.0 - ((py - dy) / dh) * 2.0)
            } else {
                Vec2::ZERO
            }
        }
    };
    // a look lasts 5 s, then back to the camera
    if s.look.0 != Look::Camera && now.duration_since(s.look.1) > Duration::from_secs(5) {
        s.look = (Look::Camera, now);
    }
    let gaze = (look + s.saccade.0).clamp(Vec2::splat(-1.0), Vec2::splat(1.0));
    // the head: breathing, a slow sway, turned a little towards the gaze, livelier while talking
    let talk = if voice.is_some() { 1.0 } else { 0.0 };
    let mut yaw = (t * 0.31).sin() * 2.5 + gaze.x * 8.0 + (t * 2.3).sin() * 1.5 * talk;
    let mut pitch = (t * 1.2).sin() * 1.2 + gaze.y * 5.0 + open * 3.0;
    let roll = (t * 0.23).sin() * 1.5 + tilt;
    if let Some(n) = s.nod {
        let p = now.duration_since(n).as_secs_f32() / 0.45;
        if p >= 1.0 {
            s.nod = None;
        } else {
            pitch -= (p * std::f32::consts::PI).sin() * 6.0;
        }
    }
    yaw = yaw.clamp(-25.0, 25.0);
    pitch = pitch.clamp(-20.0, 20.0);
    let head = Quat::from_euler(glam::EulerRot::YXZ, yaw.to_radians(), (-pitch).to_radians(), roll.to_radians());
    let face = Face { head, eye_open: [eye; 2], gaze, brow, mouth_open: open, mouth_wide: wide };
    (Some(face), expression)
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
        assert_eq!((face.head, face.gaze, face.eye_open), (camera().head, camera().gaze, camera().eye_open), "head and eyes the camera's");

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
