//! Lightair's own compositor as the window and the outputs see it: the program drawn from the
//! document, handed to the previews and to Lightair's outputs once the GPU has finished it, in
//! the order it was drawn, and a scene shown in a preview by its ids. macOS and Windows (where
//! the compositor runs); skipped without a GPU.
//!
//! The compositor is one per process (the show it reads, its hub), so these tests take turns.
#![cfg(any(target_os = "macos", windows))]

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lightair_model::{Command, Document, Fit, Id, Layout, Object, Scene, Source, Workspace};
use lightair_preview::{Frame, Preview, frame_rgba, frame_size};
use serde_json::json;

const W: u32 = 320;
const H: u32 = 180;

const RED: [u8; 3] = [220, 40, 40];
const GREEN: [u8; 3] = [40, 200, 60];
const BLUE: [u8; 3] = [40, 70, 220];
const YELLOW: [u8; 3] = [230, 210, 40];
const WHITE: [u8; 3] = [240, 240, 240];
const ORANGE: [u8; 3] = [240, 130, 20];

static TURN: Mutex<()> = Mutex::new(());

/// The compositor's turn, or `None` without a GPU (the test is skipped).
fn turn() -> Option<std::sync::MutexGuard<'static, ()>> {
    if lightair_render::shared_gpu().is_err() {
        eprintln!("no GPU here: skipped");
        return None;
    }
    let guard = TURN.lock().unwrap_or_else(|e| e.into_inner());
    lightair_host::show::set_own();
    Some(guard)
}

/// A colour source's colour (`0xAABBGGRR`).
fn colour(c: [u8; 3]) -> u32 {
    0xFF00_0000 | (c[2] as u32) << 16 | (c[1] as u32) << 8 | c[0] as u32
}

fn png(path: &Path, w: u32, h: u32, at: impl Fn(u32, u32) -> [u8; 3]) {
    image::RgbImage::from_fn(w, h, |x, y| image::Rgb(at(x, y))).save(path).unwrap();
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lightair-compositor-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The document the compositor draws: a canvas of `W`×`H`, its id, and the document.
fn document() -> (Document, Id) {
    let mut ws = Workspace::new("Compositor");
    ws.canvases[0].width = W;
    ws.canvases[0].height = H;
    let doc = Document::new(ws);
    let canvas = doc.workspace().primary_canvas().id;
    (doc, canvas)
}

fn stretched(x: f32, y: f32, w: f32, h: f32) -> Layout {
    Layout { fit: Fit::Stretch, ..Layout::boxed(x, y, w, h) }
}

/// Adds a scene of these sources (each stretched into its box), returning its id.
fn scene(doc: &mut Document, canvas: Id, name: &str, sources: Vec<(Source, [f32; 4])>) -> Id {
    let mut scene = Scene::new(name);
    let id = scene.id;
    for (source, [x, y, w, h]) in sources {
        scene.objects.push(Object::new(source.name.clone(), source.id, [(canvas, stretched(x, y, w, h))]));
        doc.apply(&Command::AddSource { source }).unwrap();
    }
    doc.apply(&Command::AddScene { scene, index: None }).unwrap();
    id
}

fn colour_source(name: &str, c: [u8; 3]) -> Source {
    Source::new(name, "color_source_v3", json!({"color": colour(c), "width": 64, "height": 64}))
}

/// What the engine does after each change: the show the compositors draw.
fn publish(doc: &Document) {
    lightair_host::show::publish(doc.workspace());
}

/// A frame as RGBA at its own size.
fn rgba(frame: &Frame) -> Option<(usize, usize, Vec<u8>)> {
    let (w, _) = frame_size(frame);
    frame_rgba(frame, w)
}

fn pixel(img: &(usize, usize, Vec<u8>), fx: f32, fy: f32) -> [u8; 3] {
    let (w, h, px) = img;
    let (x, y) = (((*w as f32 * fx) as usize).min(w - 1), ((*h as f32 * fy) as usize).min(h - 1));
    let i = (y * w + x) * 4;
    [px[i], px[i + 1], px[i + 2]]
}

fn near(c: [u8; 3], want: [u8; 3]) -> bool {
    c.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= 40)
}

/// The newest frame `preview` got since `seen`, as RGBA, if it's the canvas's size.
fn next(preview: &Preview, seen: &mut u64) -> Option<(usize, usize, Vec<u8>)> {
    let n = preview.frames();
    let frame = preview.latest_frame()?;
    if n == *seen {
        return None;
    }
    *seen = n;
    let img = rgba(&frame)?;
    (img.0 == W as usize && img.1 == H as usize).then_some(img)
}

/// Waits (10 s at most) for three frames in a row that `ok` accepts; the last frame seen
/// otherwise.
fn until(preview: &Preview, ok: impl Fn(&(usize, usize, Vec<u8>)) -> bool) -> Result<(), Option<[u8; 3]>> {
    let end = Instant::now() + Duration::from_secs(10);
    let (mut seen, mut good, mut last) = (0, 0, None);
    while Instant::now() < end {
        if let Some(img) = next(preview, &mut seen) {
            good = if ok(&img) { good + 1 } else { 0 };
            last = Some(pixel(&img, 0.5, 0.5));
            if good >= 3 {
                return Ok(());
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    Err(last)
}

/// One of Lightair's pictures as a frame the tests can read.
fn read_back(buffer: &lightair_host::buffers::Buffer) -> Option<Frame> {
    #[cfg(target_os = "macos")]
    return Some(buffer.clone());
    #[cfg(windows)]
    return lightair_preview::frame_of_buffer(buffer);
}

/// The program's preview, drawn by the compositor.
fn composed() -> Preview {
    let preview = Preview::attach();
    assert!(preview.compose_program(), "the compositor starts");
    preview
}

/// The program as placed in the document, in the window's preview and in the picture Lightair's
/// outputs take: each corner of a stretched background its own colour (a picture turned or
/// mirrored shows), and a two-tone square the right way up. Both are complete pictures, the
/// GPU's work finished before they're handed on.
#[test]
fn the_preview_and_the_outputs_get_the_program_as_placed() {
    let Some(_turn) = turn() else { return };
    let dir = scratch("placed");
    let (quarters, square) = (dir.join("quarters.png"), dir.join("square.png"));
    png(&quarters, 64, 36, |x, y| match (x < 32, y < 18) {
        (true, true) => RED,
        (false, true) => GREEN,
        (true, false) => BLUE,
        (false, false) => YELLOW,
    });
    png(&square, 100, 100, |_, y| if y < 50 { WHITE } else { ORANGE });
    let (mut doc, canvas) = document();
    let image = |name: &str, file: &Path| Source::new(name, "image_source", json!({"file": file.to_string_lossy()}));
    let main = scene(
        &mut doc,
        canvas,
        "Main",
        vec![
            (image("Quarters", &quarters), [0.0, 0.0, W as f32, H as f32]),
            (image("Square", &square), [130.0, 60.0, 60.0, 60.0]),
        ],
    );
    doc.apply(&Command::SetProgram { scene: Some(main) }).unwrap();
    publish(&doc);

    let places: [((f32, f32), [u8; 3], &str); 6] = [
        ((0.1, 0.1), RED, "top left"),
        ((0.9, 0.1), GREEN, "top right"),
        ((0.1, 0.9), BLUE, "bottom left"),
        ((0.9, 0.9), YELLOW, "bottom right"),
        ((0.5, 70.0 / H as f32), WHITE, "the square's top half"),
        ((0.5, 110.0 / H as f32), ORANGE, "the square's bottom half"),
    ];
    let right = |img: &(usize, usize, Vec<u8>)| places.iter().all(|((x, y), want, _)| near(pixel(img, *x, *y), *want));
    let described = |img: &(usize, usize, Vec<u8>)| -> String {
        places
            .iter()
            .map(|((x, y), want, what)| format!("{what}: {:?} (want {want:?}); ", pixel(img, *x, *y)))
            .collect()
    };

    let preview = composed();
    if let Err(last) = until(&preview, right) {
        let shown = preview.latest_frame().and_then(|f| rgba(&f)).map(|i| described(&i));
        panic!("the preview never showed the program (centre {last:?}): {shown:?}");
    }
    // what the outputs take: the same picture, at the canvas's size
    let program = lightair_host::program::newest(canvas, true).expect("the program was handed to the outputs");
    let img = read_back(&program).and_then(|f| rgba(&f)).expect("the outputs' picture reads back");
    assert_eq!((img.0, img.1), (W as usize, H as usize), "the outputs get the canvas's size");
    assert!(right(&img), "the outputs' picture: {}", described(&img));
    let _ = std::fs::remove_dir_all(dir);
}

/// Frames reach a preview in the order they were drawn: once the program has switched to
/// another scene, no frame of the scene before comes after (a hand-over that let a slower
/// frame overtake would show the old picture again).
#[test]
fn frames_reach_the_preview_in_the_order_drawn() {
    let Some(_turn) = turn() else { return };
    let (mut doc, canvas) = document();
    let red = scene(&mut doc, canvas, "Red", vec![(colour_source("Red", RED), [0.0, 0.0, W as f32, H as f32])]);
    let blue = scene(&mut doc, canvas, "Blue", vec![(colour_source("Blue", BLUE), [0.0, 0.0, W as f32, H as f32])]);
    doc.apply(&Command::SetProgram { scene: Some(red) }).unwrap();
    publish(&doc);
    let preview = composed();
    until(&preview, |img| near(pixel(img, 0.5, 0.5), RED)).unwrap_or_else(|last| panic!("never red: {last:?}"));

    doc.apply(&Command::SetProgram { scene: Some(blue) }).unwrap();
    publish(&doc);
    let end = Instant::now() + Duration::from_secs(2);
    let (mut seen, mut shown) = (0, Vec::new());
    while Instant::now() < end {
        if let Some(img) = next(&preview, &mut seen) {
            shown.push(if near(pixel(&img, 0.5, 0.5), BLUE) { 'B' } else { 'R' });
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    let order: String = shown.iter().collect();
    assert!(order.contains('B'), "the switch reached the preview: {order}");
    let first_blue = order.find('B').unwrap();
    assert!(!order[first_blue..].contains('R'), "a frame of the old scene came after the new one: {order}");
}

/// A preview showing a scene (the Scenes panel's, a multiview's) gets that scene, by its id and
/// its canvas's, not the program.
#[test]
fn a_preview_shows_the_scene_it_is_given() {
    let Some(_turn) = turn() else { return };
    let (mut doc, canvas) = document();
    let on_air = scene(&mut doc, canvas, "On air", vec![(colour_source("Red", RED), [0.0, 0.0, W as f32, H as f32])]);
    let next_up = scene(
        &mut doc,
        canvas,
        "Next",
        vec![
            (colour_source("Green", GREEN), [0.0, 0.0, W as f32 / 2.0, H as f32]),
            (colour_source("Yellow", YELLOW), [W as f32 / 2.0, 0.0, W as f32 / 2.0, H as f32]),
        ],
    );
    doc.apply(&Command::SetProgram { scene: Some(on_air) }).unwrap();
    publish(&doc);

    let preview = Preview::attach();
    preview.set_source(Some((next_up, canvas)));
    let ok = until(&preview, |img| near(pixel(img, 0.25, 0.5), GREEN) && near(pixel(img, 0.75, 0.5), YELLOW));
    assert!(ok.is_ok(), "the preview shows the scene it was given, left green and right yellow: {ok:?}");
    assert_eq!(preview.source(), Some((next_up, canvas)));
}
