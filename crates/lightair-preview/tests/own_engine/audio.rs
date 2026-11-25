//! Sound with the switch on: the canvases keep their channels, so libobs mixes what's on air as
//! it did, and the outputs take that mix.

use std::path::Path;
use std::process::Command as Proc;
use std::time::{Duration, Instant};

use lightair_engine::{Engine, EngineConfig};
use lightair_model::{
    Command, Document, Fit, Id, Layout, Object, Scene, Show, Source, TransitionKind, TransitionSettings, Workspace,
};
use lightair_preview::Preview;
use serde_json::json;

use crate::common::{H, SMALL, W, apply, switch_on};
use crate::record;

fn ffmpeg(args: &[&str], out: &Path) {
    let ok = Proc::new("ffmpeg").args(["-v", "error", "-y"]).args(args).arg(out).status().is_ok_and(|s| s.success());
    assert!(ok, "ffmpeg writes {}", out.display());
}

fn media(name: &str, file: &Path) -> Source {
    Source::new(
        name,
        "ffmpeg_source",
        json!({"local_file": file.to_string_lossy(), "looping": true, "is_local_file": true}),
    )
}

fn grey() -> Source {
    Source::new("Grey", "color_source_v3", json!({"color": 0xFF808080u32, "width": 64, "height": 64}))
}

fn full(canvas: Id) -> [(Id, Layout); 1] {
    [(canvas, Layout { fit: Fit::Stretch, ..Layout::boxed(0.0, 0.0, W as f32, H as f32) })]
}

fn engine() -> (Engine, Document, Id) {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
    let engine = Engine::start(EngineConfig { canvas: (W, H), output: (W, H), ..Default::default() }).unwrap();
    let mut ws = Workspace::new("Sound");
    ws.canvases[0].width = W;
    ws.canvases[0].height = H;
    let doc = Document::new(ws);
    let c = doc.workspace().primary_canvas().id;
    (engine, doc, c)
}

/// The main canvas's preview with the compositor drawing it, as the window has it.
fn composed(engine: &Engine) -> Preview {
    let preview = engine.call(|_| Preview::attach());
    preview.set_size(Some(SMALL));
    assert!(preview.compose_program(), "the compositor starts");
    preview
}

fn temp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("lightair-own-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_scenes_media_sound_is_in_the_recording_while_the_scene_is_on_air() {
    if lightair_engine::testing::ran_in_own_process() {
        return;
    }
    switch_on();
    if !record::tools() {
        return;
    }
    let dir = temp("tone");
    let wav = dir.join("tone.wav");
    ffmpeg(&["-f", "lavfi", "-i", "sine=frequency=1000:sample_rate=48000:duration=2", "-ac", "2"], &wav);
    let tone_level = record::level(&record::sound(&wav), 0.0, 2.0);
    let (engine, mut doc, c) = engine();
    let (bg, tone) = (grey(), media("Tone", &wav));
    // A shows the grey and plays the tone (no mixer track: libobs mixes it); B only the grey
    let mut a = Scene::new("A");
    a.objects.push(Object::new("Grey", bg.id, full(c)));
    a.objects.push(Object::new("Tone", tone.id, full(c)));
    let mut b = Scene::new("B");
    b.objects.push(Object::new("Grey", bg.id, full(c)));
    let (a_id, b_id) = (a.id, b.id);
    let cut = TransitionSettings { kind: TransitionKind::Cut, ..Default::default() };
    for cmd in [
        Command::AddSource { source: bg },
        Command::AddSource { source: tone },
        Command::AddScene { scene: a, index: None },
        Command::AddScene { scene: b, index: None },
        Command::SetTransition { settings: cut },
        Command::SetProgram { scene: Some(a_id) },
    ] {
        apply(&engine, &mut doc, cmd);
    }
    let preview = composed(&engine);
    std::thread::sleep(Duration::from_millis(500));

    let path = dir.join("take.mp4");
    record::start(&engine, c, &path);
    let started = Instant::now();
    std::thread::sleep(Duration::from_millis(1500));
    apply(&engine, &mut doc, Command::SetProgram { scene: Some(b_id) });
    let switched = started.elapsed().as_secs_f64();
    std::thread::sleep(Duration::from_millis(1500));
    record::stop(&engine);

    let sound = record::sound(&path);
    let end = sound.len() as f64 / 48_000.0;
    let on_air = record::level(&sound, 0.3, switched - 0.3);
    let off_air = record::level(&sound, switched + 0.5, end - 0.1);
    eprintln!(
        "the tone: {tone_level:.1} dBFS; A on air: {on_air:.1} dBFS; B on air: {off_air:.1} dBFS (switched at \
         {switched:.2} s of {end:.2} s)"
    );
    assert!((on_air - tone_level).abs() < 3.0, "the tone at its level while A is on air: {on_air:.1} dBFS");
    assert!(off_air < -50.0, "silence once B is on air: {off_air:.1} dBFS");
    drop(preview);
    drop(engine);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_source_in_an_overlay_stays_active() {
    if lightair_engine::testing::ran_in_own_process() {
        return;
    }
    switch_on();
    let (engine, mut doc, c) = engine();
    let (bg, logo) =
        (grey(), Source::new("Logo", "color_source_v3", json!({"color": 0xFF00FF00u32, "width": 64, "height": 64})));
    let mut main = Scene::new("Main");
    main.objects.push(Object::new("Grey", bg.id, full(c)));
    let mut over = Scene::new("Logo");
    over.objects.push(Object::new("Logo", logo.id, [(c, Layout::boxed(10.0, 10.0, 60.0, 60.0))]));
    let (main_id, over_id, logo_id) = (main.id, over.id, logo.id);
    for cmd in [
        Command::AddSource { source: bg },
        Command::AddSource { source: logo },
        Command::AddScene { scene: main, index: None },
        Command::AddScene { scene: over, index: None },
        Command::SetProgram { scene: Some(main_id) },
    ] {
        apply(&engine, &mut doc, cmd);
    }
    let preview = composed(&engine);
    let active = |engine: &Engine, want: bool| {
        let end = Instant::now() + Duration::from_secs(3);
        loop {
            let now = engine.call(move |s| s.reconciler.source_active(logo_id));
            if now == want || Instant::now() > end {
                return now;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    assert!(!active(&engine, false), "not shown anywhere: inactive");
    let set = |doc: &mut Document, overlays: Vec<Id>| {
        let show = Show { overlays, ..doc.workspace().show.clone() };
        apply(&engine, doc, Command::SetShow { show, label: "Overlays".into() });
    };
    set(&mut doc, vec![over_id]);
    assert!(active(&engine, true), "over every scene: active");
    set(&mut doc, vec![]);
    assert!(!active(&engine, false), "taken out: inactive again");
    drop(preview);
    drop(engine);
}
