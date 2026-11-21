//! A destination whose server goes away reconnects on its own schedule: the status carries the
//! try and when it comes, Cancel ends the retries (Idle, not Failed). Needs ffmpeg (as the RTMP
//! server).

use std::process::{Child, Command as Proc, Stdio};
use std::time::{Duration, Instant};

use lightair_engine::outputs::{OutputKey, OutputState, StreamConfig, default_encoders, runtime};
use lightair_engine::{Engine, EngineConfig};
use lightair_model::Id;

fn have(tool: &str) -> bool {
    Proc::new(tool).arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn wait(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < limit {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

#[test]
fn a_dropped_stream_counts_down_and_cancel_stops_the_retries() {
    if lightair_engine::testing::ran_in_own_process() {
        return;
    }
    if !have("ffmpeg") {
        eprintln!("ffmpeg not installed; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("lightair-reconnect-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::start(EngineConfig { canvas: (640, 360), output: (640, 360), ..Default::default() }).unwrap();
    let mut server: Child = Proc::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-listen", "1", "-i", "rtmp://127.0.0.1:19371/live/test"])
        .args(["-c", "copy", "-f", "flv"])
        .arg(dir.join("out.flv"))
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("ffmpeg starts");
    std::thread::sleep(Duration::from_millis(800));
    let caps = engine.capabilities.encoders.clone();
    let id = Id::new();
    let cfg = StreamConfig {
        id,
        server: "rtmp://127.0.0.1:19371/live".into(),
        key: "test".into(),
        encoders: default_encoders(&caps, 1500, true),
        // three tries, five seconds apart: long enough to see the countdown
        reconnect: Some((3, 5)),
        vod_mix: None,
    };
    engine.call(move |s| s.outputs.start_stream(&cfg, None)).expect("stream starts");
    assert!(wait(Duration::from_secs(6), || matches!(runtime().get(OutputKey::Stream(id)).state, OutputState::Active { .. })));
    assert_eq!(runtime().get(OutputKey::Stream(id)).retries, 3);

    // the server goes away
    let _ = server.kill();
    let _ = server.wait();
    assert!(
        wait(Duration::from_secs(10), || matches!(runtime().get(OutputKey::Stream(id)).state, OutputState::Reconnecting { .. })),
        "reconnecting after the server went away"
    );
    let st = runtime().get(OutputKey::Stream(id));
    let info = st.reconnect.expect("the status says which try comes and when");
    assert_eq!((info.attempt, info.retries), (1, 3));
    assert!(info.seconds_left() <= 5);

    // Cancel: the retries end and it's idle (not failed)
    engine.call(move |s| s.outputs.stop_stream(id));
    assert!(wait(Duration::from_secs(5), || !runtime().get(OutputKey::Stream(id)).state.is_running()), "cancel stops it");
    let st = runtime().get(OutputKey::Stream(id));
    assert_eq!(st.state, OutputState::Idle, "cancelled, not failed");
    assert!(st.reconnect.is_none());
    drop(engine);
    let _ = std::fs::remove_dir_all(dir);
}
