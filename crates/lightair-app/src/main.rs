//! Lightair application entry point.
//!
//! M2: a native GPUI window showing the live program canvas rendered by libobs, with zero-copy
//! preview (IMPLEMENTATION.md). The studio shell arrives in M4.

use std::path::PathBuf;
use std::sync::OnceLock;

use gpui_kit::*;
use lightair_engine::obs::{Data, Scene, Source};
use lightair_engine::{Engine, EngineConfig};
use lightair_preview::Preview;
use parking_lot::Mutex;

/// The engine lives until the app quits; GPUI's run loop owns everything else.
static ENGINE: OnceLock<Mutex<Option<Engine>>> = OnceLock::new();

const CANVAS: (u32, u32) = (1920, 1080);

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("LIGHTAIR_LOG").unwrap_or_else(|_| "info,libobs=warn".into()))
        .init();

    let engine = Engine::start(EngineConfig { canvas: CANVAS, output: CANVAS, ..Default::default() })
        .unwrap_or_else(|e| panic!("could not start the engine: {e}"));
    tracing::info!(version = engine.version, "libobs started");
    engine.call(build_demo_scene);
    let preview = Preview::attach();
    ENGINE.get_or_init(|| Mutex::new(Some(engine)));

    gpui_kit::application().run(move |cx: &mut App| {
        gpui_kit::init(cx);
        cx.on_window_closed(|cx, _| cx.quit()).detach();
        cx.on_app_quit(|_| async {
            if let Some(engine) = ENGINE.get().and_then(|e| e.lock().take()) {
                drop(engine);
            }
        })
        .detach();

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1280.), px(800.)), cx))),
            titlebar: Some(TitlebarOptions { title: Some("Lightair".into()), ..Default::default() }),
            ..Default::default()
        };
        gpui_kit::open_window(options, cx, |_, cx| cx.new(|_| CanvasView { preview: preview.clone() }))
            .expect("open window");
        cx.activate(true);
    });
}

/// Background, the main display, and the Lightair icon: enough to see live compositing.
fn build_demo_scene(state: &mut lightair_engine::EngineState) {
    let (w, h) = (CANVAS.0 as f32, CANVAS.1 as f32);
    let Some(scene) = Scene::create("Demo") else { return };

    let mut bg = Data::new();
    bg.set_int("color", 0xFF1B1B1C_u32 as i64).set_int("width", CANVAS.0 as i64).set_int("height", CANVAS.1 as i64);
    if let Some(src) = Source::create("color_source_v3", "Background", Some(&bg)) {
        scene.add(&src);
        state.sources.insert("background".into(), src);
    }

    // ScreenCaptureKit display capture; shows nothing until Screen Recording is allowed.
    let mut display = Data::new();
    display.set_int("type", 0).set_bool("show_cursor", true).set_bool("hide_obs", true);
    if let Some(uuid) = lightair_engine::displays::main_display_uuid() {
        display.set_str("display_uuid", &uuid);
    }
    if let Some(src) = Source::create("screen_capture", "Display", Some(&display)) {
        scene.add_fitted(&src, w * 0.05, h * 0.08, w * 0.9, h * 0.84);
        state.sources.insert("display".into(), src);
    }

    let icon = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../branding/final/icon/lightair-icon-256.png");
    let mut img = Data::new();
    img.set_str("file", &icon.to_string_lossy());
    if let Some(src) = Source::create("image_source", "Lightair icon", Some(&img)) {
        scene.add_fitted(&src, w - 200.0, h - 200.0, 160.0, 160.0);
        state.sources.insert("icon".into(), src);
    }

    state.scenes.insert("demo".into(), scene);
    state.set_program("demo");
}

struct CanvasView {
    preview: Preview,
}

impl Render for CanvasView {
    fn render(&mut self, window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // The canvas is live video: repaint every display frame.
        window.request_animation_frame();
        let canvas = match self.preview.latest_frame() {
            Some(frame) => surface(frame).size_full().object_fit(ObjectFit::Contain).into_any_element(),
            None => div().text_color(rgb(0xA1A1A6)).child("Starting preview…").into_any_element(),
        };
        div()
            .size_full()
            .bg(rgb(0x0E0E0F))
            .flex()
            .items_center()
            .justify_center()
            .p_4()
            .child(canvas)
    }
}
