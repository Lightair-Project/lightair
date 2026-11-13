//! Interface channels: an 8-input device, one track per channel (or a pair): each track hears
//! only its channel(s).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use lightair_core::{Command, Document, Input, Project, Route, Track, TrackChannels, diff};
use lightair_engine::{Engine, EngineConfig};
use serde_json::json;

#[test]
fn each_track_takes_its_channels() {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
    let engine = Engine::start(EngineConfig { canvas: (320, 180), output: (320, 180), ..Default::default() }).unwrap();
    let mut doc = Document::new(Project::new("Interface"));
    let apply = |doc: &mut Document, cmd: Command| {
        let change = doc.apply(&cmd).unwrap();
        let patches = diff(&change.before, &change.after);
        let project = change.after.clone();
        engine.call(move |s| s.reconciler.apply(&project, &patches));
    };
    // an interface input (no such device here: the test feeds it)
    let iface = Input::new("Interface", "lightair_audio_device", json!({"device": "Lightair test interface"}));
    let iid = iface.id;
    let mut vox = Track::new("Vox", iid);
    vox.channels = TrackChannels::Mono(5);
    let vox_id = vox.id;
    let mut keys = Track::new("Keys", iid);
    keys.channels = TrackChannels::Stereo(2, 7);
    keys.route = Route::None; // only measured on its own strip
    let keys_id = keys.id;
    apply(&mut doc, Command::AddInput { input: iface });
    apply(&mut doc, Command::AddTrack { track: vox, index: None });
    apply(&mut doc, Command::AddTrack { track: keys, index: None });

    // eight channels, channel k at (k + 1) × 0.05
    let mut feed = engine.call(move |s| s.reconciler.audio_test_input(iid, 8)).expect("engine running");
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let feeder = std::thread::spawn(move || {
        while !s.load(Ordering::Relaxed) {
            for _ in 0..256 {
                for k in 0..8 {
                    let _ = feed.push((k + 1) as f32 * 0.05);
                }
            }
            std::thread::sleep(Duration::from_micros(5_333));
        }
    });
    std::thread::sleep(Duration::from_millis(800));
    let read = |id| engine.call(move |s| s.reconciler.audio_meter(id).map(|m| m.read())).unwrap();
    let _ = (read(vox_id), read(keys_id)); // start new peak windows
    std::thread::sleep(Duration::from_millis(300));
    let (v, k) = (read(vox_id), read(keys_id));
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
    let lin = |db: f32| 10f32.powf(db / 20.0);
    eprintln!("vox {:?}, keys {:?}", v.peak_db, k.peak_db);
    assert!((lin(v.peak_db[0]) - 0.30).abs() < 0.005 && (lin(v.peak_db[1]) - 0.30).abs() < 0.005, "vox: channel 6 on both sides");
    assert!((lin(k.peak_db[0]) - 0.15).abs() < 0.005, "keys left: channel 3");
    assert!((lin(k.peak_db[1]) - 0.40).abs() < 0.005, "keys right: channel 8");
    drop(engine);
}
