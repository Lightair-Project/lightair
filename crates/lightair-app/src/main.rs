//! Lightair application entry point.

mod assets;
mod camera_studio;
mod editor;
mod effect_drag;
mod encoding;
mod eyedropper;
mod frame_view;
mod geometry;
mod hotkeys;
mod library;
mod mixer;
#[cfg(target_os = "macos")]
mod plugin_editor;
#[cfg(windows)]
#[path = "plugin_editor_win.rs"]
mod plugin_editor;
mod plugin_scan;
mod outputs_panel;
mod platforms;
mod preflight;
mod prefs;
mod reframe;
mod rows;
mod safe_areas;
mod secrets;
mod selftest;
mod shell;
mod starter;
mod studio;
#[cfg(target_os = "macos")]
mod system_effects;
#[cfg(windows)]
#[path = "system_effects_win.rs"]
mod system_effects;
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
    lightair_platform::paths::projects_dir()
}

fn main() {
    // a helper process listing a VST3 bundle's effects (plugin_scan)
    if let Some(code) = plugin_scan::probe_main() {
        std::process::exit(code);
    }
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

    // This Mac's preferences: the sample rate is fixed once libobs starts.
    let prefs = prefs::load_from(&prefs::path());
    prefs::LOUDNESS_TARGET.store(prefs.loudness_target, std::sync::atomic::Ordering::Relaxed);
    let sample_rate = if prefs.sample_rate == 44_100 { 44_100 } else { 48_000 };
    prefs::RUNNING_SAMPLE_RATE.store(sample_rate, std::sync::atomic::Ordering::Relaxed);
    let engine = Arc::new(
        Engine::start(EngineConfig {
            canvas,
            output: canvas,
            sample_rate,
            extra_plugin_dirs: camera_studio::plugin_dir().into_iter().collect(),
            ..Default::default()
        })
        .unwrap_or_else(|e| panic!("could not start the engine: {e}")),
    );
    tracing::info!(version = engine.version, project = %project_dir.display(), "libobs started");
    if let Some((name, id)) = prefs.monitor_device.clone() {
        engine.call(move |_| lightair_engine::monitoring::set(&name, &id));
    }
    let monitor = prefs.direct_monitor.then(|| prefs.monitor_device.clone().map_or(String::new(), |(name, _)| name));
    let program = prefs.monitor_program;
    let block = prefs.audio_block;
    engine.call(move |s| {
        s.reconciler.set_monitor(monitor, program);
        // before the engine starts: nothing to restart
        s.reconciler.set_audio_block(block, &lightair_core::Project::new(""));
    });
    if let Ok(what) = std::env::var("LIGHTAIR_SELFTEST") {
        selftest::run(&engine, &what);
        return;
    }
    let preview = Preview::attach();

    gpui_kit::application().with_assets(assets::Assets).run(move |cx: &mut App| {
        gpui_kit::init(cx);
        // VST3 plugins are made, edited and torn down on this thread
        // (GPUI's foreground executor: the main thread on both platforms)
        let (jobs, mut queue) = futures_channel::mpsc::unbounded::<Box<dyn FnOnce() + Send>>();
        lightair_engine::plugins::set_main_thread(move |job| {
            let _ = jobs.unbounded_send(job);
        });
        cx.foreground_executor()
            .spawn(async move {
                use futures_util::StreamExt;
                while let Some(job) = queue.next().await {
                    job();
                }
            })
            .detach();
        frame_view::init(cx);
        ui::apply_theme(cx);
        shell::bind_keys(cx);

        let starter: fn() -> lightair_core::Project = match std::env::var("LIGHTAIR_STARTER").as_deref() {
            Ok("plain") => starter::plain_project,
            _ => starter::starter_project,
        };
        let studio = cx.new(|_| Studio::open(engine.clone(), project_dir.clone(), starter));

        // Stats at 1 Hz; journal flush and periodic compaction.
        let ticker = studio.downgrade();
        cx.spawn(async move |cx| {
            let mut ticks = 0u32;
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                ticks += 1;
                let alive = ticker.update(cx, |s, cx| {
                    s.refresh_stats();
                    s.refit_imported(cx);
                    s.run_timer_ends(cx);
                    s.refresh_plugins();
                    s.end_replays(cx);
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

        // A copy started by a test or screenshot hook stays in the background: it must never take
        // the keyboard from the app someone is typing in.
        let background = std::env::var_os("LIGHTAIR_OPEN").is_some();
        let options = WindowOptions {
            focus: !background,
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
        if !background {
            cx.activate(true);
        }
    });
}
