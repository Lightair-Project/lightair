//! The canvases' outputs (recording here; streaming, clips, the virtual camera and NDI take the
//! same video) carry the compositor's picture of each canvas, through a private libobs canvas
//! per canvas showing a `lightair_program` source.
//!
//! Each test moves a box in libobs's own scene only (`record::move_in_libobs`), and first checks
//! libobs's own picture has it there: the compositor draws the box where the document has it, so
//! where a recording shows it says who drew the recording.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use lightair_engine::{Engine, EngineConfig};
use lightair_model::{Canvas, Command, Document, Fit, Id, Layout, Object, Scene, Source, Workspace};
use lightair_preview::Preview;
use serde_json::json;

use crate::common::{H, SMALL, W, apply, near, switch_on, until_at};
use crate::record::{self, Video, move_in_libobs};

const GREY: [u8; 3] = [128, 128, 128];
const RED: [u8; 3] = [220, 40, 40];
const BLUE: [u8; 3] = [40, 70, 220];
const YELLOW: [u8; 3] = [230, 210, 40];

/// The extra canvas: portrait.
const TALL: (u32, u32) = (180, 320);
/// libobs's box is moved this far: right in the main canvas, up in the tall one.
const MOVE: f32 = 200.0;
/// Where the box is in the main canvas (the document's place, the compositor's), and where
/// libobs's moved one is.
const LEFT: (u32, u32) = (60, 90);
const RIGHT: (u32, u32) = (260, 90);
/// The same in the tall canvas.
const BOTTOM: (u32, u32) = (90, 260);
const TOP: (u32, u32) = (90, 60);
/// Frames before this (seconds into a recording) may still be libobs's: the compositor starts
/// drawing for an output a little after it starts.
const SETTLED: f64 = 0.6;

/// `0xAABBGGRR`, as the colour source takes it.
fn abgr([r, g, b]: [u8; 3]) -> u32 {
    0xFF00_0000 | (b as u32) << 16 | (g as u32) << 8 | r as u32
}

fn colour(name: &str, c: [u8; 3], size: u32) -> Source {
    Source::new(name, "color_source_v3", json!({"color": abgr(c), "width": size, "height": size}))
}

fn stretched(x: f32, y: f32, w: f32, h: f32) -> Layout {
    Layout { fit: Fit::Stretch, ..Layout::boxed(x, y, w, h) }
}

/// libobs started at the canvas's size, and on air a grey background with a red box: at
/// [`LEFT`] in the main canvas and, with `tall`, at [`BOTTOM`] in a portrait canvas.
struct Stage {
    engine: Engine,
    doc: Document,
    primary: Id,
    tall: Option<Id>,
    scene: Id,
    red: Id,
    dir: PathBuf,
}

fn stage(name: &str, tall: bool, extra: Option<Source>) -> Stage {
    let _ = tracing_subscriber::fmt().with_env_filter("warn").with_test_writer().try_init();
    let dir = std::env::temp_dir().join(format!("lightair-own-out-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::start(EngineConfig { canvas: (W, H), output: (W, H), ..Default::default() }).unwrap();
    let mut ws = Workspace::new("Outputs");
    ws.canvases[0].width = W;
    ws.canvases[0].height = H;
    let mut doc = Document::new(ws);
    let primary = doc.workspace().primary_canvas().id;
    let tall = tall.then(|| {
        let canvas = Canvas::custom("Tall", TALL.0, TALL.1);
        let id = canvas.id;
        apply(&engine, &mut doc, Command::AddCanvas { canvas });
        id
    });
    let (bg, red) = (colour("Grey", GREY, 64), colour("Red", RED, 80));
    let mut scene = Scene::new("Main");
    let mut bg_at = vec![(primary, stretched(0.0, 0.0, W as f32, H as f32))];
    let mut red_at = vec![(primary, Layout::boxed(20.0, 50.0, 80.0, 80.0))];
    if let Some(t) = tall {
        bg_at.push((t, stretched(0.0, 0.0, TALL.0 as f32, TALL.1 as f32)));
        red_at.push((t, Layout::boxed(50.0, 220.0, 80.0, 80.0)));
    }
    scene.objects.push(Object::new("Grey", bg.id, bg_at));
    scene.objects.push(Object::new("Red", red.id, red_at));
    if let Some(source) = &extra {
        // top right of the main canvas, clear of both boxes
        scene.objects.push(Object::new("Extra", source.id, [(primary, stretched(220.0, 2.0, 80.0, 45.0))]));
    }
    let (sid, red_id) = (scene.id, red.id);
    for source in [Some(bg), Some(red), extra].into_iter().flatten() {
        apply(&engine, &mut doc, Command::AddSource { source });
    }
    apply(&engine, &mut doc, Command::AddScene { scene, index: None });
    apply(&engine, &mut doc, Command::SetProgram { scene: Some(sid) });
    Stage { engine, doc, primary, tall, scene: sid, red: red_id, dir }
}

impl Stage {
    /// Moves the box in libobs's own scenes, and waits until libobs's own picture of each canvas
    /// has it there.
    fn move_libobs_box(&self) {
        move_in_libobs(&self.engine, self.scene, self.primary, self.red, MOVE, 0.0);
        let libobs = self.engine.call(|_| Preview::attach());
        until_at(&libobs, (W, H), Duration::from_secs(10), |f| {
            near(px(f, W, RIGHT), RED) && near(px(f, W, LEFT), GREY)
        })
        .unwrap_or_else(|last| {
            panic!("libobs draws its box moved: {:?}", last.map(|f| (px(&f, W, LEFT), px(&f, W, RIGHT))))
        });
        drop(libobs);
        if let Some(t) = self.tall {
            move_in_libobs(&self.engine, self.scene, t, self.red, 0.0, -MOVE);
            let canvas = self.engine.call(move |s| s.reconciler.canvas_ref(t)).expect("the tall canvas");
            let libobs = self.engine.call(move |_| Preview::attach_canvas(canvas));
            until_at(&libobs, TALL, Duration::from_secs(10), |f| {
                near(px(f, TALL.0, TOP), RED) && near(px(f, TALL.0, BOTTOM), GREY)
            })
            .unwrap_or_else(|last| {
                panic!(
                    "libobs draws the tall box moved: {:?}",
                    last.map(|f| (px(&f, TALL.0, TOP), px(&f, TALL.0, BOTTOM)))
                )
            });
        }
    }

    /// The main canvas's preview with the compositor drawing it, as the window has it.
    fn composed(&self) -> Preview {
        let preview = self.engine.call(|_| Preview::attach());
        preview.set_size(Some(SMALL));
        assert!(preview.compose_program(), "the compositor starts");
        preview
    }

    /// Records the canvases for `long`, then reads each file back.
    fn record(&self, canvases: &[Id], long: Duration) -> Vec<Video> {
        let paths: Vec<PathBuf> = canvases.iter().map(|c| self.dir.join(format!("{c}.mp4"))).collect();
        for (c, p) in canvases.iter().zip(&paths) {
            record::start(&self.engine, *c, p);
        }
        std::thread::sleep(long);
        record::stop(&self.engine);
        paths.iter().map(|p| record::video(p)).collect()
    }

    /// Shuts the engine down, failing (rather than hanging) if it doesn't finish.
    fn end(self) {
        let Stage { engine, dir, .. } = self;
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(engine);
            let _ = done.send(());
        });
        let shut = finished.recv_timeout(Duration::from_secs(30)).is_ok();
        let _ = std::fs::remove_dir_all(dir);
        assert!(shut, "the engine shuts down");
    }
}

/// The colour of an RGBA preview frame `w` wide at `at`.
fn px(img: &[u8], w: u32, (x, y): (u32, u32)) -> [u8; 3] {
    let i = ((y * w + x) * 4) as usize;
    [img[i], img[i + 1], img[i + 2]]
}

/// The settled frames of `v` that don't show red at `red` and grey at `grey`: (time, colour at
/// `red`, colour at `grey`).
fn wrong(v: &Video, red: (u32, u32), grey: (u32, u32)) -> Vec<(f64, [u8; 3], [u8; 3])> {
    v.after(SETTLED)
        .map(|(t, f)| (*t, v.at(f, red.0, red.1), v.at(f, grey.0, grey.1)))
        .filter(|(_, r, g)| !(near(*r, RED) && near(*g, GREY)))
        .collect()
}

#[test]
fn a_recording_of_the_primary_canvas_carries_the_compositors_picture() {
    if lightair_engine::testing::ran_in_own_process() {
        return;
    }
    switch_on();
    if !record::tools() {
        return;
    }
    let stage = stage("primary", false, None);
    let preview = stage.composed();
    stage.move_libobs_box();
    let v = stage.record(&[stage.primary], Duration::from_secs(2)).remove(0);
    assert_eq!((v.w, v.h), (W, H), "the canvas's size");
    assert!(v.duration() > 1.5, "two seconds of video: {:.2}", v.duration());
    let bad = wrong(&v, LEFT, RIGHT);
    assert!(bad.is_empty(), "frames without the box where the compositor draws it (t, there, libobs's place): {bad:?}");
    drop(preview);
    stage.end();
}

#[test]
fn an_extra_canvass_recording_carries_that_canvass_compositor_picture() {
    if lightair_engine::testing::ran_in_own_process() {
        return;
    }
    switch_on();
    if !record::tools() {
        return;
    }
    let stage = stage("extra", true, None);
    let tall = stage.tall.unwrap();
    // each canvas's preview composed, as the window has them
    let main = stage.composed();
    let canvas = stage.engine.call(move |s| s.reconciler.canvas_ref(tall)).expect("the tall canvas");
    let side = stage.engine.call(move |_| Preview::attach_canvas(canvas));
    side.set_size(Some((90, 160)));
    assert!(side.compose_program(), "the tall canvas's compositor starts");
    stage.move_libobs_box();
    let v = stage.record(&[tall], Duration::from_secs(2)).remove(0);
    assert_eq!((v.w, v.h), TALL, "the tall canvas's size");
    assert!(v.duration() > 1.5, "two seconds of video: {:.2}", v.duration());
    // the main canvas's picture, squeezed in, would have grey at the bottom and its box at the left
    let bad = wrong(&v, BOTTOM, TOP);
    assert!(bad.is_empty(), "frames without the tall canvas's box where the compositor draws it: {bad:?}");
    drop((main, side));
    stage.end();
}

#[test]
fn with_the_preview_hidden_a_recording_keeps_getting_new_compositor_frames() {
    if lightair_engine::testing::ran_in_own_process() {
        return;
    }
    switch_on();
    if !record::tools() {
        return;
    }
    let dir = std::env::temp_dir().join(format!("lightair-own-hue-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let clip = dir.join("hue.mp4");
    record::hue_clip(&clip);
    let hue = Source::new(
        "Hue",
        "ffmpeg_source",
        json!({"local_file": clip.to_string_lossy(), "looping": true, "is_local_file": true}),
    );
    let stage = stage("hidden", false, Some(hue));
    let preview = stage.composed();
    stage.move_libobs_box();
    // the window's preview hidden: nothing on screen asks the compositor for frames
    preview.set_active(false);
    let v = stage.record(&[stage.primary], Duration::from_secs(3)).remove(0);
    assert!(v.duration() > 2.5, "three seconds of video: {:.2}", v.duration());
    let bad = wrong(&v, LEFT, RIGHT);
    assert!(bad.is_empty(), "frames without the box where the compositor draws it: {bad:?}");
    // the clip's colour every quarter second: each different from the one before (the clip turns
    // through the hue circle in two seconds), so the picture isn't stuck
    let clip_at = (260, 24);
    let samples: Vec<(f64, [u8; 3])> = (4..=10)
        .map(|q| q as f64 * 0.25)
        .filter_map(|t| v.frames.iter().find(|(ft, _)| *ft >= t).map(|(ft, f)| (*ft, v.at(f, clip_at.0, clip_at.1))))
        .collect();
    assert!(samples.len() >= 6, "{samples:?}");
    let stuck: Vec<_> = samples
        .windows(2)
        .filter(|w| w[0].1.iter().zip(w[1].1).map(|(a, b)| a.abs_diff(b) as u32).sum::<u32>() < 30)
        .collect();
    assert!(stuck.is_empty(), "the clip keeps changing in the recording: {samples:?}");
    drop(preview);
    stage.end();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn with_no_compositor_every_output_shows_libobs_picture_of_the_program() {
    if lightair_engine::testing::ran_in_own_process() {
        return;
    }
    switch_on();
    if !record::tools() {
        return;
    }
    // no preview asks for the compositor: it never starts
    let stage = stage("fallback", true, None);
    let tall = stage.tall.unwrap();
    stage.move_libobs_box();
    let videos = stage.record(&[stage.primary, tall], Duration::from_secs(2));
    let (main, side) = (&videos[0], &videos[1]);
    assert!(main.duration() > 1.5 && side.duration() > 1.5, "{:.2}, {:.2}", main.duration(), side.duration());
    // libobs's picture, box moved: neither black nor the document's place
    let bad = wrong(main, RIGHT, LEFT);
    assert!(bad.is_empty(), "main canvas frames without libobs's picture: {bad:?}");
    let bad = wrong(side, TOP, BOTTOM);
    assert!(bad.is_empty(), "tall canvas frames without libobs's picture: {bad:?}");
    stage.end();
}

#[test]
fn a_canvas_keeps_its_size_while_its_output_records() {
    if lightair_engine::testing::ran_in_own_process() {
        return;
    }
    switch_on();
    if !record::tools() {
        return;
    }
    let mut stage = stage("busy-resize", false, None);
    let preview = stage.composed();
    let path = stage.dir.join("take.mp4");
    record::start(&stage.engine, stage.primary, &path);
    std::thread::sleep(Duration::from_millis(1000));
    let canvas = Canvas { width: 640, height: 360, ..stage.doc.workspace().primary_canvas().clone() };
    apply(&stage.engine, &mut stage.doc, Command::UpdateCanvas { canvas });
    let libobs_size = stage.engine.call(|_| unsafe {
        let mut ovi = std::mem::zeroed::<lightair_engine::sys::obs_video_info>();
        lightair_engine::sys::obs_get_video_info(&mut ovi);
        (ovi.base_width, ovi.base_height)
    });
    std::thread::sleep(Duration::from_millis(1000));
    let running = matches!(
        lightair_engine::outputs::runtime().get(lightair_engine::outputs::OutputKey::Recording(stage.primary)).state,
        lightair_engine::outputs::OutputState::Active { .. }
    );
    record::stop(&stage.engine);
    assert_eq!(libobs_size, (W, H), "the canvas wasn't resized under its recording");
    assert!(running, "the recording carried on");
    let v = record::video(&path);
    assert_eq!((v.w, v.h), (W, H), "the recording kept its size");
    assert!(v.duration() > 1.6, "it ran to the end: {:.2}", v.duration());
    drop(preview);
    stage.end();
}

#[test]
fn a_canvas_resized_while_nothing_records_records_at_its_new_size() {
    if lightair_engine::testing::ran_in_own_process() {
        return;
    }
    switch_on();
    if !record::tools() {
        return;
    }
    let mut stage = stage("resize", true, None);
    let tall = stage.tall.unwrap();
    let preview = stage.composed();
    // something recorded each canvas before, and stopped
    let _ = stage.record(&[stage.primary, tall], Duration::from_millis(500));
    let ws = stage.doc.workspace();
    let wide = Canvas { width: 640, height: 360, ..ws.primary_canvas().clone() };
    let taller = Canvas { width: 360, height: 640, ..ws.canvas(tall).unwrap().clone() };
    apply(&stage.engine, &mut stage.doc, Command::UpdateCanvas { canvas: wide });
    apply(&stage.engine, &mut stage.doc, Command::UpdateCanvas { canvas: taller });
    let videos = stage.record(&[stage.primary, tall], Duration::from_secs(2));
    let (main, side) = (&videos[0], &videos[1]);
    assert_eq!((main.w, main.h), (640, 360), "the main canvas's new size");
    assert_eq!((side.w, side.h), (360, 640), "the tall canvas's new size");
    // the boxes where the document has them (in canvas pixels: the canvases grew around them)
    let lost = |v: &Video, at: (u32, u32)| -> Vec<(f64, [u8; 3])> {
        v.after(SETTLED).map(|(t, f)| (*t, v.at(f, at.0, at.1))).filter(|(_, c)| !near(*c, RED)).collect()
    };
    let bad = lost(main, LEFT);
    assert!(bad.is_empty(), "the main canvas's box: {bad:?}");
    let bad = lost(side, BOTTOM);
    assert!(bad.is_empty(), "the tall canvas's box: {bad:?}");
    drop(preview);
    stage.end();
}

#[test]
fn opening_another_workspace_keeps_the_primary_output_and_shows_its_program() {
    if lightair_engine::testing::ran_in_own_process() {
        return;
    }
    switch_on();
    if !record::tools() {
        return;
    }
    let stage = stage("reopen", false, None);
    let preview = stage.composed();
    let path = stage.dir.join("take.mp4");
    record::start(&stage.engine, stage.primary, &path);
    std::thread::sleep(Duration::from_millis(1000));

    // another workspace, opened as the app opens one: blue, a yellow box at the left
    let mut ws = Workspace::new("Another");
    ws.canvases[0].width = W;
    ws.canvases[0].height = H;
    let mut other = Document::new(ws);
    let c = other.workspace().primary_canvas().id;
    let (bg, yellow) = (colour("Blue", BLUE, 64), colour("Yellow", YELLOW, 80));
    let mut scene = Scene::new("Other");
    scene.objects.push(Object::new("Blue", bg.id, [(c, stretched(0.0, 0.0, W as f32, H as f32))]));
    scene.objects.push(Object::new("Yellow", yellow.id, [(c, Layout::boxed(20.0, 50.0, 80.0, 80.0))]));
    let (sid, yid) = (scene.id, yellow.id);
    for cmd in [
        Command::AddSource { source: bg },
        Command::AddSource { source: yellow },
        Command::AddScene { scene, index: None },
        Command::SetProgram { scene: Some(sid) },
    ] {
        other.apply(&cmd).unwrap();
    }
    let ws = other.workspace().clone();
    let opened = Instant::now();
    stage.engine.call(move |s| s.reconciler.rebuild(&ws));
    move_in_libobs(&stage.engine, sid, c, yid, MOVE, 0.0);
    std::thread::sleep(Duration::from_millis(2000));
    let reopened = opened.elapsed().as_secs_f64();
    let running = matches!(
        lightair_engine::outputs::runtime().get(lightair_engine::outputs::OutputKey::Recording(stage.primary)).state,
        lightair_engine::outputs::OutputState::Active { .. }
    );
    record::stop(&stage.engine);
    assert!(running, "the recording carried on");

    let v = record::video(&path);
    let end = v.duration();
    assert!(end > reopened + 0.8, "video after the other workspace opened: {end:.2} s ({reopened:.2} s of it after)");
    // the last second: the new program, the box where the document has it (the compositor's)
    let bad: Vec<_> = v
        .after(end - 1.0)
        .map(|(t, f)| (*t, v.at(f, LEFT.0, LEFT.1), v.at(f, RIGHT.0, RIGHT.1)))
        .filter(|(_, l, r)| !(near(*l, YELLOW) && near(*r, BLUE)))
        .collect();
    assert!(bad.is_empty(), "frames without the new workspace's program as the compositor draws it: {bad:?}");
    drop(preview);
    stage.end();
}

#[test]
fn opening_a_workspace_of_another_size_records_its_canvases_at_their_sizes() {
    if lightair_engine::testing::ran_in_own_process() {
        return;
    }
    switch_on();
    if !record::tools() {
        return;
    }
    let stage = stage("reopen-size", true, None);
    let preview = stage.composed();
    // something recorded the canvases before, and stopped
    let _ = stage.record(&[stage.primary, stage.tall.unwrap()], Duration::from_millis(500));

    // another workspace, opened as the app opens one: a wider main canvas and a tall one, blue,
    // a yellow box at the left of the main canvas and the bottom of the tall one
    const WIDE: (u32, u32) = (640, 360);
    let mut ws = Workspace::new("Wider");
    ws.canvases[0].width = WIDE.0;
    ws.canvases[0].height = WIDE.1;
    let mut other = Document::new(ws);
    let c = other.workspace().primary_canvas().id;
    let canvas = Canvas::custom("Tall", TALL.0, TALL.1);
    let t = canvas.id;
    let (bg, yellow) = (colour("Blue", BLUE, 64), colour("Yellow", YELLOW, 80));
    let mut scene = Scene::new("Other");
    scene.objects.push(Object::new(
        "Blue",
        bg.id,
        [
            (c, stretched(0.0, 0.0, WIDE.0 as f32, WIDE.1 as f32)),
            (t, stretched(0.0, 0.0, TALL.0 as f32, TALL.1 as f32)),
        ],
    ));
    scene.objects.push(Object::new(
        "Yellow",
        yellow.id,
        [(c, Layout::boxed(20.0, 50.0, 80.0, 80.0)), (t, Layout::boxed(50.0, 220.0, 80.0, 80.0))],
    ));
    let (sid, yid) = (scene.id, yellow.id);
    for cmd in [
        Command::AddCanvas { canvas },
        Command::AddSource { source: bg },
        Command::AddSource { source: yellow },
        Command::AddScene { scene, index: None },
        Command::SetProgram { scene: Some(sid) },
    ] {
        other.apply(&cmd).unwrap();
    }
    let ws = other.workspace().clone();
    stage.engine.call(move |s| s.reconciler.rebuild(&ws));

    // libobs's own box moved in the main canvas only, so the main recording says who drew it:
    // libobs's picture hasn't the box where the document has it
    move_in_libobs(&stage.engine, sid, c, yid, MOVE, 0.0);
    let libobs = stage.engine.call(|_| Preview::attach());
    until_at(&libobs, WIDE, Duration::from_secs(10), |f| near(px(f, WIDE.0, LEFT), BLUE)).unwrap_or_else(|last| {
        panic!("libobs draws the new canvas, box moved: {:?}", last.map(|f| px(&f, WIDE.0, LEFT)))
    });
    drop(libobs);
    until_at(&preview, WIDE, Duration::from_secs(10), |f| near(px(f, WIDE.0, LEFT), YELLOW)).unwrap_or_else(|last| {
        panic!(
            "the compositor draws the new program: {:?}",
            last.map(|f| (px(&f, WIDE.0, LEFT), px(&f, WIDE.0, RIGHT)))
        )
    });

    let videos = stage.record(&[c, t], Duration::from_secs(2));
    let (main, side) = (&videos[0], &videos[1]);
    assert_eq!((main.w, main.h), WIDE, "the new main canvas's size");
    assert_eq!((side.w, side.h), TALL, "the new tall canvas's size");
    assert!(main.duration() > 1.5 && side.duration() > 1.5, "{:.2}, {:.2}", main.duration(), side.duration());
    let shows = |v: &Video, yellow: (u32, u32), blue: (u32, u32)| -> Vec<(f64, [u8; 3], [u8; 3])> {
        v.after(SETTLED)
            .map(|(t, f)| (*t, v.at(f, yellow.0, yellow.1), v.at(f, blue.0, blue.1)))
            .filter(|(_, y, b)| !(near(*y, YELLOW) && near(*b, BLUE)))
            .collect()
    };
    let bad = shows(main, LEFT, RIGHT);
    assert!(bad.is_empty(), "main canvas frames without the new program as the compositor draws it: {bad:?}");
    let bad = shows(side, BOTTOM, TOP);
    assert!(bad.is_empty(), "tall canvas frames without the new program: {bad:?}");
    drop(preview);
    stage.end();
}
