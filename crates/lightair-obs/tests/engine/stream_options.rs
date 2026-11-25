//! A destination's own way out: bound to a local address, and held back by a delay that drains
//! when it stops (or is cut short by a forced stop). Needs ffmpeg (as the RTMP server).

use std::process::{Child, Command as Proc, Stdio};
use std::time::{Duration, Instant};

use lightair_obs::outputs::{
    OutputKey, OutputState, StreamConfig, StreamNet, default_encoders, runtime,
};
use lightair_obs::{Engine, EngineConfig};
use lightair_model::Id;

fn have(tool: &str) -> bool {
    Proc::new(tool).arg("-version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
}

fn server(port: u16, out: &std::path::Path) -> Child {
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
fn a_stream_binds_to_an_address_and_a_delay_holds_it_back() {
    if lightair_obs::testing::ran_in_own_process() {
        return;
    }
    if !have("ffmpeg") {
        eprintln!("ffmpeg not installed; skipping");
        return;
    }
    let dir = std::env::temp_dir().join(format!("lightair-options-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::start(EngineConfig { canvas: (640, 360), output: (640, 360), ..Default::default() }).unwrap();
    let servers = [server(19381, &dir.join("a.flv")), server(19382, &dir.join("b.flv"))];
    std::thread::sleep(Duration::from_millis(800));
    let caps = engine.capabilities.encoders.clone();
    let config = |port: u16, net: StreamNet| StreamConfig {
        id: Id::new(),
        server: format!("rtmp://127.0.0.1:{port}/live"),
        key: "test".into(),
        encoders: default_encoders(&caps, 1500, true),
        reconnect: Some((0, 2)),
        vod_mix: None,
        net,
    };

    // bound to the loopback address: it gets there
    let bound = config(19381, StreamNet { bind_ip: Some("127.0.0.1".into()), ..Default::default() });
    let bound_id = bound.id;
    engine.call(move |s| s.outputs.start_stream(&bound, None)).expect("bound stream starts");
    assert!(
        wait(Duration::from_secs(6), || matches!(runtime().get(OutputKey::Stream(bound_id)).state, OutputState::Active { .. })),
        "a stream bound to 127.0.0.1 connects"
    );
    engine.call(move |s| s.outputs.stop_stream(bound_id));

    // held back 3 s: connected at once, but the picture isn't sent until the delay is up
    let delayed = config(19382, StreamNet { delay: Some((3, false)), ..Default::default() });
    let id = delayed.id;
    engine.call(move |s| s.outputs.start_stream(&delayed, None)).expect("delayed stream starts");
    assert!(wait(Duration::from_secs(6), || matches!(runtime().get(OutputKey::Stream(id)).state, OutputState::Active { .. })));
    std::thread::sleep(Duration::from_millis(1200));
    engine.call(|s| s.outputs.sample());
    let early = runtime().get(OutputKey::Stream(id));
    assert_eq!(early.delay_s, 3, "the delay in force is reported");
    std::thread::sleep(Duration::from_millis(4500));
    engine.call(|s| s.outputs.sample());
    let later = runtime().get(OutputKey::Stream(id));
    eprintln!("bytes sent: {} after 1.2 s, {} after 5.7 s", early.bytes, later.bytes);
    assert!(later.bytes > early.bytes + 50_000, "the picture follows once the delay is up");

    // stopping drains the delay; a forced stop doesn't wait
    engine.call(move |s| s.outputs.stop_stream(id));
    std::thread::sleep(Duration::from_millis(500));
    let st = runtime().get(OutputKey::Stream(id));
    assert_eq!(st.state, OutputState::Stopping, "a delayed stream keeps sending what it held back");
    assert!(st.stopping_since.is_some());
    engine.call(move |s| s.outputs.force_stop_stream(id));
    assert!(wait(Duration::from_secs(3), || !runtime().get(OutputKey::Stream(id)).state.is_running()), "forced stop ends it");
    for mut s in servers {
        let _ = s.kill();
        let _ = s.wait();
    }
    drop(engine);
    let _ = std::fs::remove_dir_all(dir);
}
