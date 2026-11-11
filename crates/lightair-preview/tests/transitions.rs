//! Slide, Wipe and Dip render what they should halfway through, sampled from the program.

use std::time::{Duration, Instant};

use lightair_core::{
    Command, Direction, Document, Id, Input, Layer, Layout, Project, Scene, TransitionKind, TransitionSettings, diff,
};
use lightair_engine::{Engine, EngineConfig};
use lightair_preview::{Preview, sample_ycbcr};
use serde_json::json;

fn apply(engine: &Engine, doc: &mut Document, cmd: Command) {
    let change = doc.apply(&cmd).unwrap();
    let patches = diff(&change.before, &change.after);
    let project = change.after.clone();
    engine.call(move |s| s.reconciler.apply(&project, &patches));
}

/// Samples (left quarter, right quarter, centre) of the newest frame.
fn sample(preview: &Preview) -> [(u8, u8, u8); 3] {
    let f = preview.latest_frame().expect("frames");
    [0.25, 0.75, 0.5].map(|x| sample_ycbcr(&f, x, 0.5).unwrap())
}

fn red(p: (u8, u8, u8)) -> bool {
    p.2 > 200 && p.1 < 128
}
fn blue(p: (u8, u8, u8)) -> bool {
    p.1 > 200 && p.2 < 128
}
fn black(p: (u8, u8, u8)) -> bool {
    p.0 < 50 && (110..150).contains(&p.1) && (110..150).contains(&p.2)
}

/// Switches from `from` to `to` with `settings` and samples at the midpoint.
fn midpoint(
    engine: &Engine,
    doc: &mut Document,
    preview: &Preview,
    settings: TransitionSettings,
    to: Id,
) -> [(u8, u8, u8); 3] {
    apply(engine, doc, Command::SetTransition { settings });
    let start = Instant::now();
    apply(engine, doc, Command::SetProgram { scene: Some(to) });
    let mid = Duration::from_millis(settings.duration_ms as u64 / 2);
    std::thread::sleep(mid.saturating_sub(start.elapsed()));
    let frames = preview.frames();
    while preview.frames() < frames + 1 {
        std::thread::sleep(Duration::from_millis(5));
    }
    let s = sample(preview);
    std::thread::sleep(Duration::from_millis(settings.duration_ms as u64));
    s
}

#[test]
fn directional_and_dip_transitions() {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
    let engine = Engine::start(EngineConfig { canvas: (320, 180), output: (320, 180), ..Default::default() }).unwrap();
    let mut doc = Document::new(Project::new("Transitions"));
    let f = doc.project().primary_format().id;
    let r = Input::new("Red", "color_source_v3", json!({"color": 0xFF0000FFu32, "width": 320, "height": 180}));
    let b = Input::new("Blue", "color_source_v3", json!({"color": 0xFFFF0000u32, "width": 320, "height": 180}));
    let mut a_scene = Scene::new("A");
    a_scene.layers.push(Layer::new("Red", r.id, [(f, Layout::boxed(0.0, 0.0, 320.0, 180.0))]));
    let mut b_scene = Scene::new("B");
    b_scene.layers.push(Layer::new("Blue", b.id, [(f, Layout::boxed(0.0, 0.0, 320.0, 180.0))]));
    let (a, bb) = (a_scene.id, b_scene.id);
    for cmd in [
        Command::AddInput { input: r },
        Command::AddInput { input: b },
        Command::AddScene { scene: a_scene, index: None },
        Command::AddScene { scene: b_scene, index: None },
        Command::SetProgram { scene: Some(a) },
    ] {
        apply(&engine, &mut doc, cmd);
    }
    let preview = engine.call(|_| Preview::attach());
    std::thread::sleep(Duration::from_millis(300));
    assert!(red(sample(&preview)[2]), "starts on A");

    let slide = TransitionSettings { kind: TransitionKind::Slide, duration_ms: 1600, direction: Direction::Left };
    let [l, r_, _] = midpoint(&engine, &mut doc, &preview, slide, bb);
    eprintln!("slide left mid: {l:?} {r_:?}");
    assert!(red(l) && blue(r_), "B pushes in from the right");
    assert!(blue(sample(&preview)[2]), "lands on B");

    let wipe = TransitionSettings { kind: TransitionKind::Wipe, duration_ms: 1600, direction: Direction::Right };
    let [l, r_, _] = midpoint(&engine, &mut doc, &preview, wipe, a);
    eprintln!("wipe right mid: {l:?} {r_:?}");
    assert!(red(l) && blue(r_), "A wipes in from the left");
    assert!(red(sample(&preview)[2]), "lands on A");

    let dip = TransitionSettings { kind: TransitionKind::Dip, duration_ms: 1600, ..Default::default() };
    let [_, _, c] = midpoint(&engine, &mut doc, &preview, dip, bb);
    eprintln!("dip mid: {c:?}");
    assert!(black(c), "dips through black");
    assert!(blue(sample(&preview)[2]), "lands on B");

    // Back to Fade: the fade source is restored and still works.
    let fade = TransitionSettings { kind: TransitionKind::Fade, duration_ms: 300, ..Default::default() };
    midpoint(&engine, &mut doc, &preview, fade, a);
    assert!(red(sample(&preview)[2]), "fade lands on A");
    drop(preview);
    drop(engine);
}
