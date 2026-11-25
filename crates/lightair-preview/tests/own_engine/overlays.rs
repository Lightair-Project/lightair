//! The show's overlays ("Show over every scene") as the compositor draws them: over what's on
//! air, in every canvas, during a switch too. The compositor's frames are the canvas's size, and
//! the previews ask libobs for a smaller one, so a frame's size says who drew it.

use std::time::Duration;

use lightair_obs::{Engine, EngineConfig};
use lightair_model::{
    Canvas, Command, Document, Fit, Id, Layout, Object, Scene, Show, Source, TransitionKind, TransitionSettings,
    Workspace,
};
use lightair_preview::Preview;
use serde_json::json;

use crate::common::{H, SMALL, W, apply, rgb, switch_on, until, until_at, watch};

/// The extra canvas: portrait.
const TALL: (u32, u32) = (180, 320);

fn red(c: [u8; 3]) -> bool {
    c[0] > 200 && c[1] < 60 && c[2] < 60
}
fn blue(c: [u8; 3]) -> bool {
    c[2] > 200 && c[0] < 60 && c[1] < 60
}
fn green(c: [u8; 3]) -> bool {
    c[1] > 200 && c[0] < 60 && c[2] < 60
}
/// Partway from red to blue.
fn mixed(c: [u8; 3]) -> bool {
    (50..=210).contains(&c[0]) && (50..=210).contains(&c[2]) && c[1] < 60
}

fn solid(name: &str, color: u32) -> Source {
    Source::new(name, "color_source_v3", json!({"color": color, "width": 64, "height": 64}))
}

fn stretched(x: f32, y: f32, w: f32, h: f32) -> Layout {
    Layout { fit: Fit::Stretch, ..Layout::boxed(x, y, w, h) }
}

/// The colour of an RGBA frame `w` wide at (`x`, `y`).
fn px(img: &[u8], w: u32, x: u32, y: u32) -> [u8; 3] {
    let i = ((y * w + x) * 4) as usize;
    [img[i], img[i + 1], img[i + 2]]
}

/// The overlay's green square: (10, 10) to (70, 70) in every canvas; its middle.
const SQUARE: (u32, u32) = (40, 40);

/// libobs, a red scene on air (in a tall canvas too, with `tall`), a blue one beside it, and an
/// overlay scene with a green square, not yet over anything.
struct Stage {
    engine: Engine,
    doc: Document,
    tall: Option<Id>,
    blue: Id,
    overlay: Id,
}

fn show(tall: bool) -> Stage {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
    let engine = Engine::start(EngineConfig { canvas: (W, H), output: (W, H), ..Default::default() }).unwrap();
    let mut ws = Workspace::new("Overlays");
    ws.canvases[0].width = W;
    ws.canvases[0].height = H;
    let mut doc = Document::new(ws);
    let c = doc.workspace().primary_canvas().id;
    let tall = tall.then(|| {
        let canvas = Canvas::custom("Tall", TALL.0, TALL.1);
        let id = canvas.id;
        apply(&engine, &mut doc, Command::AddCanvas { canvas });
        id
    });
    let full = |canvas: Id| {
        let (w, h) = if Some(canvas) == tall { TALL } else { (W, H) };
        (canvas, stretched(0.0, 0.0, w as f32, h as f32))
    };
    let canvases: Vec<Id> = [Some(c), tall].into_iter().flatten().collect();
    let (r, b, g) = (solid("Red", 0xFF0000FF), solid("Blue", 0xFFFF0000), solid("Green", 0xFF00FF00));
    let mut a = Scene::new("A");
    a.objects.push(Object::new("Red", r.id, canvases.iter().map(|f| full(*f))));
    let mut bb = Scene::new("B");
    bb.objects.push(Object::new("Blue", b.id, canvases.iter().map(|f| full(*f))));
    let mut over = Scene::new("Logo");
    over.objects.push(Object::new("Green", g.id, canvases.iter().map(|f| (*f, stretched(10.0, 10.0, 60.0, 60.0)))));
    let (a_id, b_id, over_id) = (a.id, bb.id, over.id);
    for cmd in [
        Command::AddSource { source: r },
        Command::AddSource { source: b },
        Command::AddSource { source: g },
        Command::AddScene { scene: a, index: None },
        Command::AddScene { scene: bb, index: None },
        Command::AddScene { scene: over, index: None },
        Command::SetProgram { scene: Some(a_id) },
    ] {
        apply(&engine, &mut doc, cmd);
    }
    Stage { engine, doc, tall, blue: b_id, overlay: over_id }
}

impl Stage {
    fn set_overlays(&mut self, overlays: Vec<Id>) {
        let show = Show { overlays, ..self.doc.workspace().show.clone() };
        apply(&self.engine, &mut self.doc, Command::SetShow { show, label: "Overlays".into() });
    }
}

#[test]
fn an_overlay_shows_over_the_program_in_every_canvas_until_taken_out() {
    if lightair_obs::testing::ran_in_own_process() {
        return;
    }
    switch_on();
    let mut s = show(true);
    let tall = s.tall.unwrap();
    let main = s.engine.call(|_| Preview::attach());
    main.set_size(Some(SMALL));
    assert!(main.compose_program(), "the compositor starts");
    let canvas = s.engine.call(move |st| st.reconciler.canvas_ref(tall)).expect("the tall canvas");
    let side = s.engine.call(move |_| Preview::attach_canvas(canvas));
    side.set_size(Some((90, 160)));
    assert!(side.compose_program(), "the tall canvas's compositor starts");

    let looks = |over: bool| {
        move |w: u32, h: u32| {
            move |f: &[u8]| {
                let square = px(f, w, SQUARE.0, SQUARE.1);
                (if over { green(square) } else { red(square) }) && red(px(f, w, w / 2, h / 2))
            }
        }
    };
    let seen = |(w, h): (u32, u32), last: Option<Vec<u8>>| {
        last.map(|f| (px(&f, w, SQUARE.0, SQUARE.1), px(&f, w, w / 2, h / 2)))
    };
    until_at(&main, (W, H), Duration::from_secs(10), looks(false)(W, H))
        .unwrap_or_else(|last| panic!("the main canvas starts on the program alone: {:?}", seen((W, H), last)));

    s.set_overlays(vec![s.overlay]);
    until_at(&main, (W, H), Duration::from_secs(10), looks(true)(W, H))
        .unwrap_or_else(|last| panic!("the overlay over the main canvas's program: {:?}", seen((W, H), last)));
    until_at(&side, TALL, Duration::from_secs(10), looks(true)(TALL.0, TALL.1))
        .unwrap_or_else(|last| panic!("the overlay over the tall canvas's program: {:?}", seen(TALL, last)));

    s.set_overlays(vec![]);
    until_at(&main, (W, H), Duration::from_secs(10), looks(false)(W, H))
        .unwrap_or_else(|last| panic!("the overlay gone from the main canvas: {:?}", seen((W, H), last)));
    until_at(&side, TALL, Duration::from_secs(10), looks(false)(TALL.0, TALL.1))
        .unwrap_or_else(|last| panic!("the overlay gone from the tall canvas: {:?}", seen(TALL, last)));
    drop((main, side));
    drop(s.engine);
}

#[test]
fn an_overlay_stays_over_both_scenes_during_a_fade() {
    if lightair_obs::testing::ran_in_own_process() {
        return;
    }
    switch_on();
    let mut s = show(false);
    let preview = s.engine.call(|_| Preview::attach());
    preview.set_size(Some(SMALL));
    assert!(preview.compose_program(), "the compositor starts");
    s.set_overlays(vec![s.overlay]);
    let square = |f: &[u8]| px(f, W, SQUARE.0, SQUARE.1);
    let centre = |f: &[u8]| rgb(f, 0.5, 0.5);
    until(&preview, Duration::from_secs(10), |f| green(square(f)) && red(centre(f)))
        .unwrap_or_else(|last| panic!("starts on A with the overlay: {:?}", last.map(|f| (square(&f), centre(&f)))));

    let fade = TransitionSettings { kind: TransitionKind::Fade, duration_ms: 1000, ..Default::default() };
    apply(&s.engine, &mut s.doc, Command::SetTransition { settings: fade });
    apply(&s.engine, &mut s.doc, Command::SetProgram { scene: Some(s.blue) });
    let seen: Vec<([u8; 3], [u8; 3])> =
        watch(&preview, Duration::from_millis(1300)).iter().map(|f| (square(f), centre(f))).collect();
    assert!(seen.iter().any(|(_, c)| mixed(*c)), "partway through, red and blue mixed: {seen:?}");
    assert!(seen.last().is_some_and(|(_, c)| blue(*c)), "lands on B: {seen:?}");
    assert!(seen.iter().all(|(sq, _)| green(*sq)), "the overlay over every frame of the fade: {seen:?}");
    drop(preview);
    drop(s.engine);
}
