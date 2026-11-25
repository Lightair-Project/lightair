//! Recordings and what's in them: starting and stopping a canvas's recording as the app does
//! (`Reconciler::canvas_video`), and reading the file back with ffprobe and ffmpeg.
//!
//! Telling the two pictures apart: libobs and the compositor draw the program alike, so a test
//! moves an object in libobs's own scene only ([`move_in_libobs`]). The compositor draws it where
//! the document has it, libobs where it was moved to, so where it shows says who drew a frame.

use std::path::Path;
use std::process::{Command as Proc, Stdio};
use std::time::{Duration, Instant};

use lightair_engine::outputs::{Container, OutputKey, OutputState, RecordingConfig, default_encoders, runtime};
use lightair_engine::{Engine, sys};
use lightair_model::Id;

pub fn have(tool: &str) -> bool {
    Proc::new(tool).arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

/// Whether ffmpeg and ffprobe are here (the recording tests skip without them).
pub fn tools() -> bool {
    let ok = have("ffmpeg") && have("ffprobe");
    if !ok {
        eprintln!("ffmpeg or ffprobe not installed; skipping");
    }
    ok
}

fn wait_for(key: OutputKey, timeout: Duration, pred: impl Fn(&OutputState) -> bool) -> OutputState {
    let start = Instant::now();
    loop {
        let state = runtime().get(key).state;
        if pred(&state) || start.elapsed() > timeout {
            return state;
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// Starts recording `canvas` to `path` (an MP4) from the video the app gives a recording, and
/// waits until it's running.
pub fn start(engine: &Engine, canvas: Id, path: &Path) {
    let cfg = RecordingConfig {
        extra_mixes: vec![],
        canvas,
        path: path.to_path_buf(),
        label: Some(canvas.to_string()),
        split_minutes: None,
        container: Container::HybridMp4,
        encoders: default_encoders(&engine.capabilities.encoders, 4000, false),
    };
    engine
        .call(move |s| {
            let video = s.reconciler.canvas_video(canvas);
            s.outputs.start_recording(&cfg, video)
        })
        .expect("the recording starts");
    let state =
        wait_for(OutputKey::Recording(canvas), Duration::from_secs(10), |s| matches!(s, OutputState::Active { .. }));
    assert!(matches!(state, OutputState::Active { .. }), "the recording runs: {state:?}");
}

/// Stops every recording and waits until they've finished writing.
pub fn stop(engine: &Engine) {
    engine.call(|s| s.outputs.stop_recording());
    let end = Instant::now() + Duration::from_secs(10);
    while runtime().any_recording_running() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(!runtime().any_recording_running(), "the recordings stop");
}

/// A recording's video: its size and every frame (RGB) with its time from the first frame.
pub struct Video {
    pub w: u32,
    pub h: u32,
    pub frames: Vec<(f64, Vec<u8>)>,
}

impl Video {
    /// The mean colour of the 7×7 pixels around (`x`, `y`) of `frame` (compression blurs edges
    /// and colours; a block's mean doesn't care).
    pub fn at(&self, frame: &[u8], x: u32, y: u32) -> [u8; 3] {
        let mut sum = [0u32; 3];
        let mut n = 0;
        for v in y.saturating_sub(3)..(y + 4).min(self.h) {
            for u in x.saturating_sub(3)..(x + 4).min(self.w) {
                let i = ((v * self.w + u) * 3) as usize;
                for c in 0..3 {
                    sum[c] += frame[i + c] as u32;
                }
                n += 1;
            }
        }
        sum.map(|s| (s / n.max(1)) as u8)
    }

    /// The frames from `from` seconds on.
    pub fn after(&self, from: f64) -> impl Iterator<Item = &(f64, Vec<u8>)> {
        self.frames.iter().filter(move |(t, _)| *t >= from)
    }

    pub fn duration(&self) -> f64 {
        self.frames.last().map_or(0.0, |(t, _)| *t)
    }
}

/// Every frame of `path`'s video, decoded by ffmpeg.
pub fn video(path: &Path) -> Video {
    let out = Proc::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=width,height", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let mut dims = text.trim().split(',').map(|v| v.trim().parse::<u32>().unwrap_or(0));
    let (w, h) = (dims.next().unwrap_or(0), dims.next().unwrap_or(0));
    assert!(w > 0 && h > 0, "{} has video: {text:?}", path.display());
    let times = Proc::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "frame=pts_time", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe runs");
    let times: Vec<f64> = String::from_utf8_lossy(&times.stdout)
        .lines()
        .filter_map(|l| l.trim().trim_end_matches(',').parse().ok())
        .collect();
    let raw = Proc::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:v:0", "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "rgb24", "-"])
        .output()
        .expect("ffmpeg runs");
    let size = (w * h * 3) as usize;
    let first = times.first().copied().unwrap_or(0.0);
    let frames = raw
        .stdout
        .chunks_exact(size)
        .enumerate()
        .map(|(i, f)| (times.get(i).map_or(0.0, |t| t - first), f.to_vec()))
        .collect();
    Video { w, h, frames }
}

/// `path`'s sound, mono, 48 kHz, from its first sample.
pub fn sound(path: &Path) -> Vec<f32> {
    let raw = Proc::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-ac", "1", "-ar", "48000", "-f", "f32le", "-"])
        .output()
        .expect("ffmpeg runs");
    raw.stdout.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect()
}

/// The level (dBFS, RMS) of `samples` (48 kHz) between `from` and `to` seconds.
pub fn level(samples: &[f32], from: f64, to: f64) -> f32 {
    let (a, b) = ((from * 48_000.0) as usize, ((to * 48_000.0) as usize).min(samples.len()));
    if a >= b {
        return f32::NEG_INFINITY;
    }
    let power = samples[a..b].iter().map(|s| s * s).sum::<f32>() / (b - a) as f32;
    10.0 * power.max(1e-12).log10()
}

/// Moves `source`'s object in libobs's own scene of `scene` in `canvas` by (`dx`, `dy`) canvas
/// pixels, leaving the document (and so the compositor) as it is.
pub fn move_in_libobs(engine: &Engine, scene: Id, canvas: Id, source: Id, dx: f32, dy: f32) {
    let moved = engine.call(move |s| unsafe {
        let (Some(scene), Some(source)) =
            (s.reconciler.scene_source(scene, canvas), s.reconciler.source_source(source))
        else {
            return false;
        };
        let item = sys::obs_scene_find_source(sys::obs_scene_from_source(scene), sys::obs_source_get_name(source));
        if item.is_null() {
            return false;
        }
        let mut pos = lightair_engine::obs::vec2(0.0, 0.0);
        sys::obs_sceneitem_get_pos(item, &mut pos);
        let (x, y) = (pos.__bindgen_anon_1.__bindgen_anon_1.x, pos.__bindgen_anon_1.__bindgen_anon_1.y);
        sys::obs_sceneitem_set_pos(item, &lightair_engine::obs::vec2(x + dx, y + dy));
        true
    });
    assert!(moved, "the object is in libobs's scene");
}

/// Writes a 2-second looping clip (160×90, 30 fps) whose colour turns round the hue circle once
/// in it: every frame a different colour.
pub fn hue_clip(path: &Path) {
    let ok = Proc::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "color=c=red:s=160x90:r=30:d=2,hue=h=t*180"])
        .args(["-c:v", "libx264", "-pix_fmt", "yuv420p"])
        .arg(path)
        .status()
        .is_ok_and(|s| s.success());
    assert!(ok, "ffmpeg writes the clip");
}
