//! Lightair application entry point.

mod mixer;
mod shell;
mod starter;
mod studio;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::*;
use lightair_engine::{Engine, EngineConfig};
use lightair_preview::Preview;

use crate::shell::StudioWindow;
use crate::studio::Studio;

fn projects_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join("Library/Application Support/Lightair/Projects")
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("LIGHTAIR_LOG").unwrap_or_else(|_| "info,libobs=warn".into()))
        .init();

    // The canvas size comes from the project's primary format; read it before starting libobs.
    let project_dir = std::env::var_os("LIGHTAIR_PROJECT")
        .map(PathBuf::from)
        .unwrap_or_else(|| projects_dir().join("Default.lightair"));
    let canvas = lightair_store::load_snapshot(&project_dir)
        .map(|p| (p.primary_format().width, p.primary_format().height))
        .unwrap_or((1920, 1080));

    let engine = Arc::new(
        Engine::start(EngineConfig { canvas, output: canvas, ..Default::default() })
            .unwrap_or_else(|e| panic!("could not start the engine: {e}")),
    );
    tracing::info!(version = engine.version, project = %project_dir.display(), "libobs started");
    let preview = Preview::attach();

    gpui_kit::application().with_assets(gpui_kit::assets::AllAssets).run(move |cx: &mut App| {
        gpui_kit::init(cx);
        ui::apply_theme(cx);
        shell::bind_keys(cx);

        let studio = cx.new(|_| Studio::open(engine.clone(), project_dir.clone(), starter::starter_project));

        // Stats at 1 Hz; journal flush and periodic compaction.
        let ticker = studio.downgrade();
        cx.spawn(async move |cx| {
            let mut ticks = 0u32;
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                ticks += 1;
                let alive = ticker.update(cx, |s, cx| {
                    s.refresh_stats();
                    s.flush();
                    if ticks.is_multiple_of(60) {
                        s.compact();
                    }
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        let on_quit = studio.clone();
        cx.on_app_quit(move |cx| {
            on_quit.update(cx, |s, _| s.compact());
            async {}
        })
        .detach();
        cx.on_window_closed(|cx, _| cx.quit()).detach();

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1440.), px(900.)), cx))),
            titlebar: Some(TitlebarOptions {
                title: Some("Lightair".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(16.), px(15.))),
            }),
            ..Default::default()
        };
        let preview = preview.clone();
        gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| StudioWindow::new(studio, preview, window, cx)))
            .expect("open window");
        cx.activate(true);
    });
}
