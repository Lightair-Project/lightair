//! Lightair application entry point.

// Windows: a window app, no console (development builds keep theirs, for the log)
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod activity_panel;
mod assets;
mod automations_panel;
mod avatar_library;
mod browser_import;
mod console;
mod stall_watch;
mod triggers;
mod updater;
mod camera_studio;
mod chat_panel;
mod chat_send;
mod editor;
mod effect_drag;
mod encoding;
mod enhanced;
mod eyedropper;
mod file_pick;
mod frame_view;
mod geometry;
mod shortcuts;
mod library;
mod machine;
mod mcp_bridge;
mod mixer;
mod native_preview;
mod now_playing;
#[cfg(target_os = "macos")]
mod plugin_editor;
#[cfg(windows)]
#[path = "plugin_editor_win.rs"]
mod plugin_editor;
mod plugin_scan;
mod pngtuber_editor;
mod outputs_panel;
mod packages;
mod permissions;
mod piair;
mod catalogs;
mod grants;
mod helpers_host;
mod links;
mod plugin_activity;
mod platforms;
mod preflight;
mod prefs;
mod reframe;
mod rows;
mod safe_areas;
mod show_panel;
mod widget_feeds;
mod midi_map;
mod ptz;
mod keys;
mod secrets;
mod shell;
mod starter;
mod storage;
mod stream_panel;
mod studio;
#[cfg(target_os = "macos")]
mod system_effects;
#[cfg(windows)]
#[path = "system_effects_win.rs"]
mod system_effects;
mod ui;
mod voice;
mod widgets;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::*;
use lightair_host::{Engine, EngineConfig};
use lightair_preview::Preview;

use crate::shell::StudioWindow;
use crate::studio::Studio;

fn workspaces_dir() -> PathBuf {
    lightair_platform::paths::workspaces_dir()
}

fn main() {
    // Chromium's own processes, when Chromium started this program for one (Windows, where
    // lightair-web-helper.exe isn't beside it)
    #[cfg(windows)]
    if let Some(code) = lightair_web::helper_main() {
        std::process::exit(code);
    }
    // a helper process listing a VST3 bundle's effects (plugin_scan)
    if let Some(code) = plugin_scan::probe_main() {
        std::process::exit(code);
    }
    // an agent's MCP connection (stdout is its, so before logging starts)
    if let Some(code) = mcp_bridge::main_if_asked() {
        std::process::exit(code);
    }
    // Chromium's library before any other thread exists (lightair_web::preload)
    lightair_web::preload();
    tracing_subscriber::fmt()
        .with_env_filter(std::env::var("LIGHTAIR_LOG").unwrap_or_else(|_| "info".into()))
        .init();
    // Chromium's profile (cookies: platforms signed in to) kept with Lightair's data
    lightair_web::set_profile_dir(lightair_platform::paths::data_dir().join("Chromium"));
    // Plugins' refused requests and permission uses, for the notices and the activity panel.
    plugin_activity::connect();
    // Catalogs: loaded, checked now and every 6 hours, their revoked lists enforced.
    catalogs::start();
    // the chat readers that need a browser (Kick, the signed-in platforms) use Lightair's
    lightair_live::pages::set_host(Arc::new(chat_panel::HiddenPages));

    // the workspace: the one asked for, the one open last, or the default
    let workspace_dir = std::env::var_os("LIGHTAIR_PROJECT")
        .map(|p| std::path::absolute(&p).unwrap_or_else(|_| PathBuf::from(p)))
        .or_else(|| prefs::load_from(&prefs::path()).last_workspace.filter(|d| lightair_store::is_workspace(d)))
        .unwrap_or_else(|| workspaces_dir().join("Default.lightair"));

    // This machine's preferences: the sample rate is fixed once the engine starts.
    let prefs = prefs::load_from(&prefs::path());
    // the remote API's password, read while the engine starts (the API answers with the window)
    if prefs.remote.enabled && (prefs.remote.require_password || prefs.remote.lan) && std::env::var_os("LIGHTAIR_REMOTE_OFF").is_none() {
        secrets::read_remote_password_early();
    }
    prefs::LOUDNESS_TARGET.store(prefs.loudness_target, std::sync::atomic::Ordering::Relaxed);
    let sample_rate = if prefs.sample_rate == 44_100 { 44_100 } else { 48_000 };
    prefs::RUNNING_SAMPLE_RATE.store(sample_rate, std::sync::atomic::Ordering::Relaxed);
    let engine = Arc::new(
        Engine::start(EngineConfig { sample_rate })
            .unwrap_or_else(|e| panic!("could not start the engine: {e}")),
    );
    tracing::info!(version = engine.version, workspace = %workspace_dir.display(), "engine started");
    if let Some((name, id)) = prefs.headphones_device.clone() {
        engine.call(move |_| lightair_host::monitoring::set(&name, &id));
    }
    let monitor = prefs.low_latency_headphones.then(|| prefs.headphones_device.clone().map_or(String::new(), |(_, id)| id));
    let program = prefs.headphones_hear_program;
    let block = prefs.audio_block;
    engine.call(move |s| {
        s.reconciler.set_headphones(monitor, program);
        // before the engine starts: nothing to restart
        s.reconciler.set_audio_block(block, &lightair_model::Workspace::new(""));
    });
    // iPhones and iPads by USB, among the cameras
    lightair_platform::devices::allow_ios_screens();
    let preview = Preview::attach();
    preview.set_rate(prefs.preview_fps);
    // Lightair's own compositor draws the program in the window
    #[cfg(any(target_os = "macos", windows))]
    if lightair_host::bridge::own() && !preview.compose_program() {
        tracing::warn!("the compositor didn't start: the program's preview stays empty");
    }

    let app = gpui_kit::application().with_assets(assets::Assets);
    // lightair:// links from the website or another app (the window handles them)
    app.on_open_urls(links::push);
    app.run(move |cx: &mut App| {
        gpui_kit::init(cx);
        // a camera (an iPhone's above all) that comes back: its captures start again
        lightair_platform::devices::watch_cameras(|uid| {
            tracing::debug!(%uid, "camera connected");
            lightair_host::on_screen::device_reconnected(&uid);
        });
        // VST3 plugins are made, edited and torn down on this thread
        // (GPUI's foreground executor: the main thread on both platforms)
        let (jobs, mut queue) = futures_channel::mpsc::unbounded::<Box<dyn FnOnce() + Send>>();
        lightair_host::plugins::set_main_thread(move |job| {
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
        // the preview under the UI shows through where the window leaves room (native_preview)
        gpui_kit::component::set_transparent_roots(native_preview::on());
        ui::apply_theme(cx);
        shell::bind_keys(cx);
        shell::app_menu::bind(cx);

        let starter: fn() -> lightair_model::Workspace = match std::env::var("LIGHTAIR_STARTER").as_deref() {
            Ok("plain") => starter::plain_workspace,
            _ => starter::starter_workspace,
        };
        let studio = cx.new(|_| Studio::open(engine.clone(), workspace_dir.clone(), starter));

        // Stats at 1 Hz; journal flush and periodic compaction.
        let ticker = studio.downgrade();
        cx.spawn(async move |cx| {
            let mut ticks = 0u32;
            loop {
                cx.background_executor().timer(Duration::from_secs(1)).await;
                ticks += 1;
                let alive = ticker.update(cx, |s, cx| {
                    s.refresh_stats(cx);
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
            // (saved before quitting: the package writes on a thread of its own)
            on_quit.update(cx, |s, _| s.save_and_wait());
            async {}
        })
        .detach();
        // closing the main window quits (a panel's own window docks it again: shell/panel_window)
        let main_window: std::rc::Rc<std::cell::Cell<Option<WindowId>>> = Default::default();
        let main_id = main_window.clone();
        cx.on_window_closed(move |cx, closed| {
            if main_id.get().is_none_or(|main| main == closed) {
                cx.quit();
            }
        })
        .detach();

        // A copy started by a test or screenshot hook stays in the background: it must never take
        // the keyboard from the app someone is typing in.
        // LIGHTAIR_STALLS=1: where the UI thread is when the window stops answering
        stall_watch::start(cx);
        // LIGHTAIR_BACKGROUND=1 (the benchmark) also stays behind the app in front
        let background = std::env::var_os("LIGHTAIR_OPEN").is_some() || std::env::var("LIGHTAIR_BACKGROUND").is_ok_and(|v| v == "1");
        // its size; a screenshot hook can ask for another ("900x700")
        let (w, h) = std::env::var("LIGHTAIR_WINDOW")
            .ok()
            .and_then(|v| v.split_once('x').and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?))))
            .unwrap_or((1440.0, 900.0));
        let options = WindowOptions {
            focus: !background,
            // the canvas preview is drawn under the UI, showing where the UI leaves room
            window_background: if native_preview::on() { WindowBackgroundAppearance::Transparent } else { WindowBackgroundAppearance::Opaque },
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(w), px(h)), cx))),
            titlebar: Some(TitlebarOptions {
                title: Some("Lightair".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(16.), px(15.))),
            }),
            ..Default::default()
        };
        let preview = preview.clone();
        let (main, _) = gpui_kit::open_window(options, cx, |window, cx| cx.new(|cx| StudioWindow::new(studio, preview, window, cx)))
            .expect("open window");
        main_window.set(Some(main.window_id()));
        if !background {
            cx.activate(true);
        }
    });
}
