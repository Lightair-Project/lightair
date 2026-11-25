//! The program as the compositor draws it from libobs's pictures of its sources: in the main
//! canvas's preview, and back in libobs through a `lightair_program` source.

use std::path::{Path, PathBuf};
use std::time::Duration;

use lightair_engine::{Engine, EngineConfig, program_source};
use lightair_model::{Command, Document, Fit, Id, Layout, Object, Scene, Source, Workspace};
use lightair_preview::Preview;
use serde_json::json;

use crate::common::{H, Rgba, SMALL, W, apply, differ, near, rgb, save, switch_on, until, watch};

const RED: [u8; 3] = [220, 40, 40];
const GREEN: [u8; 3] = [40, 200, 60];
const BLUE: [u8; 3] = [40, 70, 220];
const YELLOW: [u8; 3] = [230, 210, 40];
/// The colour source (`0xAABBGGRR` 0xFFE040C0).
const PURPLE: [u8; 3] = [0xC0, 0x40, 0xE0];
const WHITE: [u8; 3] = [240, 240, 240];
const ORANGE: [u8; 3] = [240, 130, 20];

/// An image of `w`×`h`, each pixel coloured by `at(x, y)`.
fn png(path: &Path, w: u32, h: u32, at: impl Fn(u32, u32) -> [u8; 3]) {
    let img = image::RgbImage::from_fn(w, h, |x, y| image::Rgb(at(x, y)));
    img.save(path).unwrap();
}

/// What the program should show, place by place: each corner of the background its own
/// colour (so a picture upside down or mirrored shows), the colour source in its box, and the
/// square's two halves the right way up.
fn places() -> [((f32, f32), [u8; 3], &'static str); 7] {
    [
        ((0.1, 0.1), RED, "top left"),
        ((0.9, 0.1), GREEN, "top right"),
        ((0.1, 0.9), BLUE, "bottom left"),
        ((0.9, 0.9), YELLOW, "bottom right"),
        ((0.5, 40.0 / H as f32), PURPLE, "the colour source"),
        ((0.5, 105.0 / H as f32), WHITE, "the square's top half"),
        ((0.5, 145.0 / H as f32), ORANGE, "the square's bottom half"),
    ]
}

fn looks_right(img: &[u8]) -> bool {
    places().iter().all(|((x, y), want, _)| near(rgb(img, *x, *y), *want))
}

/// What a frame shows at each place, for a failure's message.
fn described(img: &Option<Rgba>) -> String {
    match img {
        None => "no frame at the canvas's size".into(),
        Some(img) => places()
            .iter()
            .map(|((x, y), want, what)| format!("{what}: {:?} (want {want:?}); ", rgb(img, *x, *y)))
            .collect(),
    }
}

/// libobs started at the canvas's size, and a program of three sources libobs draws, placed and
/// scaled: a background image of four coloured quarters stretched to twice its size over the
/// canvas, a colour source squeezed into a wide box, and a two-tone square shrunk into a smaller
/// one.
struct Show {
    engine: Engine,
    canvas: Id,
    dir: PathBuf,
}

fn show(name: &str) -> Show {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
    let dir = std::env::temp_dir().join(format!("lightair-own-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (quarters, square) = (dir.join("quarters.png"), dir.join("square.png"));
    png(&quarters, W / 2, H / 2, |x, y| match (x < W / 4, y < H / 4) {
        (true, true) => RED,
        (false, true) => GREEN,
        (true, false) => BLUE,
        (false, false) => YELLOW,
    });
    png(&square, 100, 100, |_, y| if y < 50 { WHITE } else { ORANGE });

    let engine = Engine::start(EngineConfig { canvas: (W, H), output: (W, H), ..Default::default() }).unwrap();
    let mut ws = Workspace::new("Own engine");
    ws.canvases[0].width = W;
    ws.canvases[0].height = H;
    let mut doc = Document::new(ws);
    let c = doc.workspace().primary_canvas().id;
    let image = |name: &str, file: &PathBuf| Source::new(name, "image_source", json!({"file": file.to_string_lossy()}));
    let bg = image("Quarters", &quarters);
    let sq = image("Square", &square);
    let colour = Source::new("Colour", "color_source_v3", json!({"color": 0xFFE040C0u32, "width": 100, "height": 100}));
    let stretched = |x, y, w, h| Layout { fit: Fit::Stretch, ..Layout::boxed(x, y, w, h) };
    let mut scene = Scene::new("Main");
    let sid = scene.id;
    scene.objects.push(Object::new("Quarters", bg.id, [(c, stretched(0.0, 0.0, W as f32, H as f32))]));
    scene.objects.push(Object::new("Colour", colour.id, [(c, stretched(120.0, 20.0, 80.0, 40.0))]));
    scene.objects.push(Object::new("Square", sq.id, [(c, stretched(130.0, 95.0, 60.0, 60.0))]));
    for source in [bg, colour, sq] {
        apply(&engine, &mut doc, Command::AddSource { source });
    }
    apply(&engine, &mut doc, Command::AddScene { scene, index: None });
    apply(&engine, &mut doc, Command::SetProgram { scene: Some(sid) });
    Show { engine, canvas: c, dir }
}

/// The main canvas's preview with the compositor drawing its program (asked for at a smaller
/// size than the canvas, as a window's preview is: the compositor's frames are the canvas's
/// size, libobs's this one).
fn composed(engine: &Engine) -> Preview {
    let preview = engine.call(|_| Preview::attach());
    preview.set_size(Some(SMALL));
    assert!(preview.compose_program(), "the compositor starts");
    preview
}

#[test]
fn the_preview_shows_the_compositors_picture_of_the_program_as_libobs_draws_it() {
    if lightair_engine::testing::ran_in_own_process() {
        return;
    }
    switch_on();
    let Show { engine, dir, .. } = show("preview");
    // libobs's own frame of the program, beside it
    let libobs_view = engine.call(|_| Preview::attach());
    let preview = composed(&engine);

    let ours = until(&preview, Duration::from_secs(10), looks_right)
        .unwrap_or_else(|last| panic!("the compositor's frames never showed the program: {}", described(&last)));
    let libobs = until(&libobs_view, Duration::from_secs(10), looks_right)
        .unwrap_or_else(|last| panic!("libobs's own frame isn't the program: {}", described(&last)));
    save(&dir, "libobs.png", &libobs);
    save(&dir, "compositor.png", &ours);
    let (bad, n, worst) = differ(&libobs, &ours);
    eprintln!("own engine: {bad} of {n} samples differ (frames in {})", dir.display());
    // (edges a pixel apart and the scaling filters aside) the same picture
    assert!(bad * 100 <= n, "{bad} of {n} samples differ (libobs, compositor): {worst:?}");
    drop(preview);
    drop(libobs_view);
    drop(engine);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_lightair_program_source_shows_the_compositors_picture_in_libobs() {
    if lightair_engine::testing::ran_in_own_process() {
        return;
    }
    switch_on();
    let Show { engine, canvas, dir } = show("source");
    // libobs draws the source on its own, as it would on a canvas of its own
    let program = engine.call(move |_| program_source::create(canvas)).expect("a lightair_program source");
    let through_libobs = engine.call(|_| Preview::attach());
    through_libobs.set_source(Some(program));

    // no compositor yet: nothing to show
    let before = watch(&through_libobs, Duration::from_millis(500));
    assert!(!before.is_empty(), "libobs draws the source");
    let lit: Vec<_> = before
        .iter()
        .flat_map(|f| places().map(|((x, y), _, what)| (what, rgb(f, x, y))))
        .filter(|(_, c)| c.iter().any(|v| *v > 30))
        .collect();
    assert!(lit.is_empty(), "before the compositor's first frame it shows nothing: {lit:?}");

    let preview = composed(&engine);
    let shown = until(&through_libobs, Duration::from_secs(10), looks_right)
        .unwrap_or_else(|last| panic!("the source never showed the compositor's picture: {}", described(&last)));
    let ours = until(&preview, Duration::from_secs(10), looks_right)
        .unwrap_or_else(|last| panic!("the compositor's frames never showed the program: {}", described(&last)));
    save(&dir, "through-libobs.png", &shown);
    save(&dir, "compositor.png", &ours);
    let (bad, n, worst) = differ(&shown, &ours);
    eprintln!("lightair_program: {bad} of {n} samples differ (frames in {})", dir.display());
    assert!(bad * 100 <= n, "{bad} of {n} samples differ (through libobs, compositor): {worst:?}");
    through_libobs.set_source(None);
    drop(preview);
    drop(through_libobs);
    drop(engine);
    let _ = std::fs::remove_dir_all(dir);
}
