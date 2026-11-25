//! Media files and streams AVFoundation doesn't play (WebM, MKV, AVI, a network stream…), without
//! libobs (macOS): decoded by the `ffmpeg` tool Lightair ships (`lightair_render::media::ffmpeg`),
//! in real time, while the source is on air. Two processes per run: the pictures (raw RGBA,
//! published by the source's id for the compositor: [`crate::frames`]) and the sound (float PCM
//! at the mixer's rate, into its feed by id).
//!
//! Settings, as the media source keeps them: `local_file` (with `is_local_file`, on by default)
//! or `input` (a URL), `looping`, `restart_on_activate` (on by default: off air it stops, on air
//! it plays from the start; off: it carries on from where it was).
//!
//! Not yet: `speed_percent` (always normal speed), a rotation stored in the file (drawn as
//! stored), `clear_on_media_end`.

use std::io::Read;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use lightair_model::Id;
use parking_lot::Mutex;

use crate::media_player::{MediaStateCode, state};

/// What the source plays, from its settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub input: String,
    pub local: bool,
    pub looping: bool,
    pub restart_on_activate: bool,
}

impl Settings {
    pub fn from_json(s: &serde_json::Value) -> Settings {
        let local = s["is_local_file"].as_bool().unwrap_or(true);
        let key = if local { "local_file" } else { "input" };
        Settings {
            input: s[key].as_str().unwrap_or_default().to_string(),
            local,
            looping: s["looping"].as_bool().unwrap_or(false),
            restart_on_activate: s["restart_on_activate"].as_bool().unwrap_or(true),
        }
    }
}

/// A run of the decoders, stopped as it drops.
struct Run {
    children: Vec<Child>,
    stop: Arc<AtomicBool>,
}

impl Drop for Run {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for c in &mut self.children {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

struct Control {
    settings: Settings,
    run: Option<Run>,
    state: MediaStateCode,
    /// Where it plays from (ms into the file) and since when; where it was when paused.
    from_ms: i64,
    since: Option<Instant>,
    /// The picture's size and the file's length, once probed.
    size: (u32, u32),
    duration_ms: i64,
}

/// One media source played through FFmpeg.
pub struct FfmpegMedia {
    id: Id,
    rate: u32,
    control: Arc<Mutex<Control>>,
}

impl FfmpegMedia {
    pub fn new(id: Id, settings: Settings, rate: u32) -> FfmpegMedia {
        // its sound into a track of the mixer
        crate::audio_host::feed_slot(id);
        let control =
            Control { settings, run: None, state: state::NONE, from_ms: 0, since: None, size: (0, 0), duration_ms: 0 };
        FfmpegMedia { id, rate, control: Arc::new(Mutex::new(control)) }
    }

    /// New settings: a new file or stream plays from the start (when it's playing).
    pub fn update(&self, settings: Settings) {
        let mut c = self.control.lock();
        let reopen = c.settings.input != settings.input || c.settings.looping != settings.looping;
        c.settings = settings;
        c.size = (0, 0);
        let playing = c.run.is_some();
        drop(c);
        if reopen && playing {
            self.play_from(0);
        }
    }

    /// On air: from the start (`restart_on_activate`), or on from where it was.
    pub fn activate(&self) {
        let c = self.control.lock();
        let from = if c.settings.restart_on_activate { 0 } else { c.from_ms };
        drop(c);
        self.play_from(from);
    }

    /// Off air: stopped, or paused where it is.
    pub fn deactivate(&self) {
        let restart = self.control.lock().settings.restart_on_activate;
        if restart { self.stop() } else { self.play_pause(true) }
    }

    pub fn play_pause(&self, pause: bool) {
        if pause {
            let mut c = self.control.lock();
            c.from_ms = time_of(&c);
            c.since = None;
            c.run = None;
            if c.state == state::PLAYING {
                c.state = state::PAUSED;
            }
        } else {
            let from = self.control.lock().from_ms;
            self.play_from(from);
        }
    }

    pub fn restart(&self) {
        self.play_from(0);
    }

    pub fn stop(&self) {
        let mut c = self.control.lock();
        c.run = None;
        c.since = None;
        c.from_ms = 0;
        c.state = state::STOPPED;
        drop(c);
        crate::frames::forget(self.id);
    }

    pub fn set_time_ms(&self, ms: i64) {
        let playing = self.control.lock().run.is_some();
        if playing {
            self.play_from(ms.max(0));
        } else {
            self.control.lock().from_ms = ms.max(0);
        }
    }

    pub fn state(&self) -> MediaStateCode {
        self.control.lock().state
    }

    pub fn time_ms(&self) -> i64 {
        time_of(&self.control.lock())
    }

    pub fn duration_ms(&self) -> i64 {
        self.control.lock().duration_ms
    }

    /// The picture's size (0×0 before it's known, and for sound only).
    pub fn size(&self) -> (u32, u32) {
        self.control.lock().size
    }

    /// Starts the decoders at `from_ms`.
    fn play_from(&self, from_ms: i64) {
        let mut c = self.control.lock();
        c.run = None;
        let settings = c.settings.clone();
        if settings.input.is_empty() {
            return;
        }
        let Ok(ffmpeg) = lightair_render::media::ffmpeg() else {
            tracing::warn!(id = %self.id, "media: no ffmpeg to play it");
            c.state = state::ERROR;
            return;
        };
        if c.size == (0, 0)
            && let Ok(p) = lightair_render::media::probe(Path::new(&settings.input))
        {
            c.size = (p.width, p.height);
            c.duration_ms = p.duration_ms as i64;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let mut children = Vec::new();
        let base = |cmd: &mut Command| {
            cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-noautorotate", "-re"]);
            if settings.looping && settings.local {
                cmd.args(["-stream_loop", "-1"]);
            }
            if from_ms > 0 {
                cmd.args(["-ss", &format!("{:.3}", from_ms as f64 / 1000.0)]);
            }
            cmd.arg("-i").arg(&settings.input);
            cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null());
        };
        // the pictures
        let (w, h) = c.size;
        if w > 0 && h > 0 {
            let mut cmd = Command::new(&ffmpeg);
            base(&mut cmd);
            cmd.args(["-map", "0:v:0", "-an", "-f", "rawvideo", "-pix_fmt", "rgba", "-"]);
            if let Ok(mut child) = cmd.spawn()
                && let Some(out) = child.stdout.take()
            {
                let (id, stop, control) = (self.id, stop.clone(), self.control.clone());
                let _ = std::thread::Builder::new()
                    .name("lightair-ffmpeg-video".into())
                    .spawn(move || read_pictures(id, out, (w, h), stop, control));
                children.push(child);
            }
        }
        // the sound
        let mut cmd = Command::new(&ffmpeg);
        base(&mut cmd);
        cmd.args(["-map", "0:a:0?", "-vn", "-f", "f32le", "-ac", "2", "-ar", &self.rate.to_string(), "-"]);
        if let Ok(mut child) = cmd.spawn()
            && let Some(out) = child.stdout.take()
        {
            let (id, rate, stop) = (self.id, self.rate, stop.clone());
            let control = (w == 0 || h == 0).then(|| self.control.clone());
            let _ = std::thread::Builder::new()
                .name("lightair-ffmpeg-audio".into())
                .spawn(move || read_sound(id, out, rate, stop, control));
            children.push(child);
        }
        if children.is_empty() {
            c.state = state::ERROR;
            return;
        }
        c.run = Some(Run { children, stop });
        c.from_ms = from_ms;
        c.since = Some(Instant::now());
        c.state = state::PLAYING;
    }
}

impl Drop for FfmpegMedia {
    fn drop(&mut self) {
        self.control.lock().run = None;
        crate::frames::forget(self.id);
        crate::audio_host::feeds_itself_no_more(self.id);
    }
}

/// Where playback is now (ms into the file; wrapping round a looping file).
fn time_of(c: &Control) -> i64 {
    let t = c.from_ms + c.since.map_or(0, |s| s.elapsed().as_millis() as i64);
    match c.duration_ms {
        d if d > 0 && c.settings.looping => t % d,
        d if d > 0 => t.min(d),
        _ => t,
    }
}

/// The pictures as they come, published for the compositor; the end of a run ends playback.
fn read_pictures(
    id: Id,
    mut out: std::process::ChildStdout,
    (w, h): (u32, u32),
    stop: Arc<AtomicBool>,
    control: Arc<Mutex<Control>>,
) {
    let mut frame = vec![0u8; w as usize * h as usize * 4];
    while !stop.load(Ordering::Relaxed) {
        if out.read_exact(&mut frame).is_err() {
            break;
        }
        if let Some(buffer) = crate::buffers::from_rgba(&frame, w, h) {
            crate::frames::publish(id, buffer);
        }
    }
    ended(&stop, &control);
}

/// The sound as it comes, into the mixer's feed for `id` (sound only: its end ends playback).
fn read_sound(
    id: Id,
    mut out: std::process::ChildStdout,
    rate: u32,
    stop: Arc<AtomicBool>,
    control: Option<Arc<Mutex<Control>>>,
) {
    // 10 ms blocks, stereo
    let frames = (rate / 100).max(1) as usize;
    let mut bytes = vec![0u8; frames * 2 * 4];
    let mut samples = vec![0f32; frames * 2];
    while !stop.load(Ordering::Relaxed) {
        if out.read_exact(&mut bytes).is_err() {
            break;
        }
        for (s, b) in samples.iter_mut().zip(bytes.as_chunks::<4>().0) {
            *s = f32::from_le_bytes(*b);
        }
        if let Some(slot) = crate::audio_host::slot_of(id) {
            slot.push(&samples, 2, rate);
        }
    }
    if let Some(control) = control {
        ended(&stop, &control);
    }
}

/// A run's output ended by itself (not stopped): the file played to its end.
fn ended(stop: &AtomicBool, control: &Mutex<Control>) {
    if stop.load(Ordering::Relaxed) {
        return;
    }
    let mut c = control.lock();
    c.state = state::ENDED;
    c.from_ms = 0;
    c.since = None;
}
