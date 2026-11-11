//! Recording and streaming end to end, verified with ffprobe (and ffmpeg as a local RTMP
//! server). Skips the parts whose tools aren't installed.

use std::io::Write;
use std::path::Path;
use std::process::{Command as Proc, Stdio};
use std::time::{Duration, Instant};

use lightair_core::{Command, Document, Input, Layer, Layout, Project, Scene, Track, diff};
use lightair_engine::outputs::{
    Container, OutputKey, OutputState, RecordingConfig, StreamConfig, default_encoders, runtime,
};
use lightair_engine::{Engine, EngineConfig};
use serde_json::json;

fn have(tool: &str) -> bool {
    Proc::new(tool).arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn tone_wav(path: &Path) {
    let rate = 48_000u32;
    let mut data = Vec::new();
    for i in 0..rate * 2 {
        let s = (0.3 * (2.0 * std::f32::consts::PI * 440.0 * i as f32 / rate as f32).sin() * i16::MAX as f32) as i16;
        data.extend_from_slice(&s.to_le_bytes());
        data.extend_from_slice(&s.to_le_bytes());
    }
    let mut f = std::fs::File::create(path).unwrap();
    for chunk in [
        &b"RIFF"[..],
        &(36 + data.len() as u32).to_le_bytes(),
        b"WAVEfmt ",
        &16u32.to_le_bytes(),
        &1u16.to_le_bytes(),
        &2u16.to_le_bytes(),
        &rate.to_le_bytes(),
        &(rate * 4).to_le_bytes(),
        &4u16.to_le_bytes(),
        &16u16.to_le_bytes(),
        b"data",
        &(data.len() as u32).to_le_bytes(),
    ] {
        f.write_all(chunk).unwrap();
    }
    f.write_all(&data).unwrap();
}

/// (codec types and names, duration seconds) from ffprobe.
fn probe(path: &Path) -> (Vec<String>, f64) {
    let out = Proc::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_type,codec_name:format=duration", "-of", "json"])
        .arg(path)
        .output()
        .expect("ffprobe runs");
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let streams = v["streams"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|s| {
                    format!("{}:{}", s["codec_type"].as_str().unwrap_or(""), s["codec_name"].as_str().unwrap_or(""))
                })
                .collect()
        })
        .unwrap_or_default();
    let duration = v["format"]["duration"].as_str().and_then(|d| d.parse().ok()).unwrap_or(0.0);
    (streams, duration)
}

fn wait_for(key: OutputKey, timeout: Duration, pred: impl Fn(&OutputState) -> bool) -> OutputState {
    let start = Instant::now();
    loop {
        let state = runtime().get(key).state;
        if pred(&state) || start.elapsed() > timeout {
            return state;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn record_and_stream() {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
    if !have("ffprobe") {
        eprintln!("ffprobe not installed; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("lightair-outputs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let wav = dir.join("tone.wav");
    tone_wav(&wav);

    let engine = Engine::start(EngineConfig { canvas: (640, 360), output: (640, 360), ..Default::default() }).unwrap();
    let mut doc = Document::new(Project::new("Outputs"));
    let format = doc.project().primary_format().id;
    let colour = Input::new("Colour", "color_source_v3", json!({"color": 0xFF3B4AFFu32, "width": 640, "height": 360}));
    let tone = Input::new(
        "Tone",
        "ffmpeg_source",
        json!({"local_file": wav.to_string_lossy(), "looping": true, "is_local_file": true}),
    );
    let mut scene = Scene::new("Main");
    scene.layers.push(Layer::new("Colour", colour.id, [(format, Layout::boxed(0.0, 0.0, 640.0, 360.0))]));
    let track = Track::new("Tone", tone.id);
    for cmd in [
        Command::AddInput { input: colour },
        Command::AddInput { input: tone },
        Command::AddScene { scene, index: None },
        Command::AddTrack { track, index: None },
    ] {
        let change = doc.apply(&cmd).unwrap();
        let patches = diff(&change.before, &change.after);
        let project = change.after.clone();
        engine.call(move |s| s.reconciler.apply(&project, &patches));
    }
    let encoders = default_encoders(&engine.capabilities.encoders, 2500, false);
    eprintln!("encoders: {} + {}", encoders.video_encoder, encoders.audio_encoder);

    // ---- recording
    let file = dir.join("take.mp4");
    let cfg = RecordingConfig { path: file.clone(), container: Container::HybridMp4, encoders: encoders.clone() };
    engine.call(move |s| s.outputs.start_recording(&cfg, None)).expect("recording starts");
    let state = wait_for(OutputKey::Recording, Duration::from_secs(3), |s| matches!(s, OutputState::Active { .. }));
    assert!(matches!(state, OutputState::Active { .. }), "recording active: {state:?}");
    std::thread::sleep(Duration::from_millis(2500));
    engine.call(|s| s.outputs.sample());
    assert!(runtime().get(OutputKey::Recording).bytes > 0, "bytes written while recording");
    engine.call(|s| s.outputs.stop_recording());
    let state = wait_for(OutputKey::Recording, Duration::from_secs(5), |s| !s.is_running());
    assert_eq!(state, OutputState::Idle, "recording stopped cleanly");
    let (streams, duration) = probe(&file);
    eprintln!("recording: {streams:?}, {duration:.2} s");
    assert!(streams.iter().any(|s| s == "video:h264"), "{streams:?}");
    assert!(streams.iter().any(|s| s == "audio:aac"), "{streams:?}");
    assert!(duration > 1.8, "duration {duration}");

    // ---- streaming to a local RTMP server
    if have("ffmpeg") {
        let received = dir.join("received.flv");
        let mut server = Proc::new("ffmpeg")
            .args([
                "-nostdin",
                "-loglevel",
                "error",
                "-listen",
                "1",
                "-i",
                "rtmp://127.0.0.1:19350/live/test",
                "-c",
                "copy",
                "-f",
                "flv",
            ])
            .arg(&received)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("ffmpeg starts");
        std::thread::sleep(Duration::from_millis(700));
        let id = lightair_core::Id::new();
        let cfg = StreamConfig {
            id,
            server: "rtmp://127.0.0.1:19350/live".into(),
            key: "test".into(),
            encoders: default_encoders(&engine.capabilities.encoders, 2500, true),
        };
        engine.call(move |s| s.outputs.start_stream(&cfg, None)).expect("stream starts");
        let state =
            wait_for(OutputKey::Stream(id), Duration::from_secs(5), |s| matches!(s, OutputState::Active { .. }));
        assert!(matches!(state, OutputState::Active { .. }), "stream active: {state:?}");
        assert!(runtime().any_stream_running());
        std::thread::sleep(Duration::from_secs(3));
        engine.call(move |s| s.outputs.stop_stream(id));
        let state = wait_for(OutputKey::Stream(id), Duration::from_secs(5), |s| !s.is_running());
        assert_eq!(state, OutputState::Idle);
        let deadline = Instant::now() + Duration::from_secs(5);
        while server.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = server.kill();
        let (streams, duration) = probe(&received);
        eprintln!("server received: {streams:?}, {duration:.2} s");
        assert!(streams.iter().any(|s| s == "video:h264"), "{streams:?}");
        assert!(streams.iter().any(|s| s == "audio:aac"), "{streams:?}");
        assert!(duration > 2.0, "duration {duration}");
    } else {
        eprintln!("ffmpeg not installed; skipping the streaming half");
    }

    drop(engine);
    let _ = std::fs::remove_dir_all(dir);
}
