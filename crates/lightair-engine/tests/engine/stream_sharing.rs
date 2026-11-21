//! Destinations with identical encoder settings share one encoder; both still get the stream,
//! and stopping one leaves the other running. Needs ffmpeg (as RTMP servers) and ffprobe.

use std::path::{Path, PathBuf};
use std::process::{Child, Command as Proc, Stdio};
use std::time::{Duration, Instant};

use lightair_model::{Command, Document, Id, Source, Object, Layout, Workspace, Scene, diff};
use lightair_engine::outputs::{OutputKey, OutputState, StreamConfig, default_encoders, runtime};
use lightair_engine::{Engine, EngineConfig};
use serde_json::json;

fn have(tool: &str) -> bool {
    Proc::new(tool).arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn server(port: u16, out: &Path) -> Child {
    Proc::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-listen", "1", "-i"])
        .arg(format!("rtmp://127.0.0.1:{port}/live/test"))
        .args(["-c", "copy", "-f", "flv"])
        .arg(out)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("ffmpeg starts")
}

fn video_seconds(path: &Path) -> f64 {
    let out = Proc::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v", "-show_entries", "format=duration", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .expect("ffprobe runs");
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0.0)
}

fn wait_active(id: Id) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(6) {
        if matches!(runtime().get(OutputKey::Stream(id)).state, OutputState::Active { .. }) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

fn finish(mut child: Child) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = child.kill();
}

#[test]
fn identical_destinations_share_an_encoder() {
    if lightair_engine::testing::ran_in_own_process() {
        return;
    }
    let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
    if !have("ffmpeg") || !have("ffprobe") {
        eprintln!("ffmpeg/ffprobe not installed; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("lightair-share-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::start(EngineConfig { canvas: (640, 360), output: (640, 360), ..Default::default() }).unwrap();
    let mut doc = Document::new(Workspace::new("Share"));
    let f = doc.workspace().primary_canvas().id;
    let colour = Source::new("Colour", "color_source_v3", json!({"color": 0xFF3B4AFFu32, "width": 640, "height": 360}));
    let mut scene = Scene::new("Main");
    scene.objects.push(Object::new("Colour", colour.id, [(f, Layout::boxed(0.0, 0.0, 640.0, 360.0))]));
    for cmd in [Command::AddSource { source: colour }, Command::AddScene { scene, index: None }] {
        let change = doc.apply(&cmd).unwrap();
        let patches = diff(&change.before, &change.after);
        let workspace = change.after.clone();
        engine.call(move |s| s.reconciler.apply(&workspace, &patches));
    }

    let ports = [19361u16, 19362, 19363, 19364, 19365, 19366, 19367, 19368];
    let files: Vec<PathBuf> = ports.iter().map(|p| dir.join(format!("{p}.flv"))).collect();
    let servers: Vec<Child> = ports.iter().zip(&files).map(|(p, f)| server(*p, f)).collect();
    std::thread::sleep(Duration::from_millis(800));

    let caps = engine.capabilities.encoders.clone();
    let config = |port: u16, kbps: u32| StreamConfig {
        id: Id::new(),
        server: format!("rtmp://127.0.0.1:{port}/live"),
        key: "test".into(),
        encoders: default_encoders(&caps, kbps, true),
        reconnect: None,
        vod_mix: None,
    };
    let (a, b, c) = (config(ports[0], 2500), config(ports[1], 2500), config(ports[2], 1200));
    let ids = [a.id, b.id, c.id];
    for cfg in [a, b, c] {
        engine.call(move |s| s.outputs.start_stream(&cfg, None)).expect("stream starts");
    }
    for id in ids {
        assert!(wait_active(id), "stream {id} active");
    }
    let enc = |id: Id| engine.call(move |s| s.outputs.stream_video_encoder(id)).unwrap();
    assert_eq!(enc(ids[0]), enc(ids[1]), "same settings share an encoder");
    assert_ne!(enc(ids[0]), enc(ids[2]), "a different bitrate gets its own");
    assert_eq!(engine.call(|s| s.outputs.stream_encoder_count()), 2);

    // A destination on a mix with a VOD track (Twitch's): its sound from libobs mix 2, and a
    // second audio track from mix 1; not sharing the others' audio.
    let mut vod = config(ports[3], 2500);
    vod.encoders.audio_mix = 2;
    vod.vod_mix = Some(1);
    let vod_id = vod.id;
    engine.call(move |s| s.outputs.start_stream(&vod, None)).expect("stream with a VOD track starts");
    assert!(wait_active(vod_id), "stream with a VOD track active");
    assert_eq!(engine.call(move |s| s.outputs.stream_audio_tracks(vod_id)), vec![2, 1], "its mix, then the VOD track");
    assert_eq!(engine.call(move |s| s.outputs.stream_audio_tracks(ids[0])), vec![0], "the others: the master alone");

    // Adaptive bitrate: two identical adaptive destinations each get an encoder of their own
    // (their outputs change the bitrate), and report the bitrate they're at against the target.
    let adaptive = |port: u16| {
        let mut cfg = config(port, 2500);
        cfg.encoders.adaptive = true;
        cfg
    };
    let (d, e) = (adaptive(ports[4]), adaptive(ports[5]));
    let (d_id, e_id) = (d.id, e.id);
    for cfg in [d, e] {
        engine.call(move |s| s.outputs.start_stream(&cfg, None)).expect("adaptive stream starts");
    }
    assert!(wait_active(d_id) && wait_active(e_id), "adaptive streams active");
    assert_ne!(enc(d_id), enc(e_id), "adaptive destinations never share an encoder");
    assert_ne!(enc(d_id), enc(ids[0]), "nor with a fixed-bitrate one");
    std::thread::sleep(Duration::from_millis(1200));
    engine.call(|s| s.outputs.sample());
    let status = runtime().get(OutputKey::Stream(d_id));
    assert_eq!(status.target_kbps, 2500);
    assert!(status.current_kbps > 0 && status.current_kbps <= 2500, "reports its bitrate: {}", status.current_kbps);
    engine.call(move |s| s.outputs.stop_stream(d_id));
    engine.call(move |s| s.outputs.stop_stream(e_id));

    // A frame rate of its own: every second frame of the canvas, on an encoder of its own.
    let full = config(ports[6], 2500);
    let mut half = config(ports[7], 2500);
    half.encoders.fps_divisor = 2;
    let (full_id, half_id) = (full.id, half.id);
    for cfg in [full, half] {
        engine.call(move |s| s.outputs.start_stream(&cfg, None)).expect("stream starts");
    }
    assert!(wait_active(full_id) && wait_active(half_id), "frame-rate streams active");
    assert_ne!(enc(full_id), enc(half_id), "a different frame rate never shares an encoder");
    std::thread::sleep(Duration::from_millis(3000));
    engine.call(|s| s.outputs.sample());
    let (all, some) = (runtime().get(OutputKey::Stream(full_id)).total_frames, runtime().get(OutputKey::Stream(half_id)).total_frames);
    eprintln!("frames sent in 3 s: {all} at the canvas rate, {some} at half");
    assert!(all > 20 && some * 10 > all * 4 && some * 10 < all * 6, "about half the frames: {some} of {all}");
    engine.call(move |s| s.outputs.stop_stream(full_id));
    engine.call(move |s| s.outputs.stop_stream(half_id));

    // Stopping the first leaves the shared encoder running for the second.
    std::thread::sleep(Duration::from_millis(1200));
    let first = ids[0];
    engine.call(move |s| s.outputs.stop_stream(first));
    std::thread::sleep(Duration::from_millis(1200));
    assert!(matches!(runtime().get(OutputKey::Stream(ids[1])).state, OutputState::Active { .. }), "second still live");
    engine.call(|s| s.outputs.stop_all_streams());
    for id in &ids[1..] {
        let start = Instant::now();
        while runtime().get(OutputKey::Stream(*id)).state.is_running() && start.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    for s in servers {
        finish(s);
    }
    let secs: Vec<f64> = files.iter().map(|f| video_seconds(f)).collect();
    eprintln!("received seconds: {secs:?}");
    assert!(secs[0] > 0.5, "first destination got video before it stopped");
    assert!(secs[1] > secs[0] + 0.6, "second kept going after the first stopped");
    assert!(secs[2] > secs[0] + 0.6, "third got video");
    drop(engine);
    let _ = std::fs::remove_dir_all(dir);
}
