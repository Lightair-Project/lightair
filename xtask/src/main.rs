//! Build orchestration (docs/15-platform-distribution-licensing.md, "Building").
//!
//!   cargo xtask modules    configure and build every in-tree libobs module (modules/*)
//!   cargo xtask bundle     assemble target/debug/Lightair.app for development (macOS)
//!   cargo xtask dev        link OBS's helper programs next to target/debug for `cargo run`
//!   cargo xtask test [filter]  build every test program, then run them all at once

use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

fn run(cmd: &mut Command) -> anyhow::Result<()> {
    eprintln!("$ {cmd:?}");
    let status = cmd.status()?;
    anyhow::ensure!(status.success(), "command failed: {status}");
    Ok(())
}

fn modules() -> anyhow::Result<()> {
    let root = root();
    for entry in std::fs::read_dir(root.join("modules"))? {
        let dir = entry?.path();
        if !dir.join("CMakeLists.txt").exists() {
            continue;
        }
        let name = dir.file_name().unwrap().to_string_lossy().into_owned();
        let build = root.join("target/modules").join(&name);
        run(Command::new("cmake").arg("-S").arg(&dir).arg("-B").arg(&build).args([
            "-G",
            "Ninja",
            "-DCMAKE_BUILD_TYPE=RelWithDebInfo",
            "-DCMAKE_OSX_ARCHITECTURES=arm64",
        ]))?;
        run(Command::new("cmake").arg("--build").arg(&build))?;
    }
    Ok(())
}

const INFO_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key><string>Lightair</string>
	<key>CFBundleDisplayName</key><string>Lightair</string>
	<key>CFBundleIdentifier</key><string>app.lightair.dev</string>
	<key>CFBundleExecutable</key><string>lightair</string>
	<key>CFBundleIconFile</key><string>Lightair</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>0.1.0</string>
	<key>CFBundleVersion</key><string>1</string>
	<key>LSMinimumSystemVersion</key><string>14.0</string>
	<key>NSHighResolutionCapable</key><true/>
	<key>NSCameraUsageDescription</key><string>Lightair shows your camera in your scenes and recordings.</string>
	<key>NSMicrophoneUsageDescription</key><string>Lightair records and streams your microphone.</string>
	<key>NSLocalNetworkUsageDescription</key><string>Lightair finds network video sources and serves the remote control API.</string>
</dict>
</plist>
"#;

/// Development bundle: the debug binary inside a real .app (so macOS asks Lightair, not the
/// terminal, for camera and microphone access), with OBS's frameworks and plugins linked in
/// from an installed OBS 33 (docs/15-platform-distribution-licensing.md, "Building").
fn bundle() -> anyhow::Result<()> {
    let root = root();
    run(Command::new("cargo").current_dir(&root).args(["build", "-p", "lightair-app"]))?;
    let obs = PathBuf::from(std::env::var("OBS_APP").unwrap_or_else(|_| "/Applications/OBS.app".into()));
    let app = root.join("target/debug/Lightair.app/Contents");
    let _ = std::fs::remove_dir_all(app.parent().unwrap());
    for dir in ["MacOS", "Frameworks", "PlugIns", "Resources"] {
        std::fs::create_dir_all(app.join(dir))?;
    }
    std::fs::copy(root.join("target/debug/lightair"), app.join("MacOS/lightair"))?;
    // libobs runs this helper (from next to the executable) to write clips and MKV recordings.
    std::os::unix::fs::symlink(obs.join("Contents/MacOS/obs-ffmpeg-mux"), app.join("MacOS/obs-ffmpeg-mux"))?;
    std::fs::write(app.join("Info.plist"), INFO_PLIST)?;
    let icns = root.join("branding/final/icon/Lightair.icns");
    if icns.exists() {
        std::fs::copy(icns, app.join("Resources/Lightair.icns"))?;
    }
    for (from, to) in
        [(obs.join("Contents/Frameworks"), app.join("Frameworks")), (obs.join("Contents/PlugIns"), app.join("PlugIns"))]
    {
        for entry in std::fs::read_dir(&from)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            // OBS's CEF helpers and UI frameworks aren't used by Lightair.
            if name_str.starts_with("OBS Helper") {
                continue;
            }
            std::os::unix::fs::symlink(entry.path(), to.join(&name))?;
        }
    }
    // Sign the bundle itself so macOS privacy settings identify Lightair. Nested OBS code keeps
    // OBS's own signatures; nothing outside target/ is modified.
    //
    // Ad hoc signatures change with every build, so macOS asks for camera, microphone and screen
    // permission again after each rebuild. A real identity keeps them: the first "Apple
    // Development" identity in the keychain is used by default; LIGHTAIR_SIGN_IDENTITY overrides
    // it ("-" for ad hoc).
    let identity = std::env::var("LIGHTAIR_SIGN_IDENTITY")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(apple_development_identity)
        .unwrap_or_else(|| "-".into());
    run(Command::new("codesign").args(["--force", "--sign", &identity]).arg(app.parent().unwrap()))?;
    if identity == "-" {
        eprintln!(
            "signed ad hoc: macOS will ask for permissions again after rebuilds (set LIGHTAIR_SIGN_IDENTITY to keep them)"
        );
    } else {
        eprintln!("signed with {identity}");
    }
    eprintln!("built {}", app.parent().unwrap().display());
    Ok(())
}

/// Links OBS's helper programs next to the debug binary (development without the bundle).
fn dev() -> anyhow::Result<()> {
    let obs = PathBuf::from(std::env::var("OBS_APP").unwrap_or_else(|_| "/Applications/OBS.app".into()));
    for dir in [root().join("target/debug"), root().join("target/debug/deps")] {
        std::fs::create_dir_all(&dir)?;
        let link = dir.join("obs-ffmpeg-mux");
        if !link.exists() {
            std::os::unix::fs::symlink(obs.join("Contents/MacOS/obs-ffmpeg-mux"), &link)?;
        }
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("modules") => modules(),
        Some("bundle") => bundle(),
        Some("dev") => dev(),
        Some("test") => test(std::env::args().skip(2).collect()),
        _ => {
            eprintln!("usage: cargo xtask (modules | bundle | dev | test [filter])");
            Ok(())
        }
    }
}

/// The SHA-1 of the first valid "Apple Development" code signing identity, if any.
fn apple_development_identity() -> Option<String> {
    let out = Command::new("security").args(["find-identity", "-v", "-p", "codesigning"]).output().ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find(|l| l.contains("\"Apple Development:"))
        .and_then(|l| l.split_whitespace().nth(1))
        .filter(|h| h.len() == 40)
        .map(str::to_string)
}

/// A test program to run: its name, path, and package directory (cargo runs tests from there).
struct TestBin {
    name: String,
    exe: PathBuf,
    dir: PathBuf,
}

/// `cargo test` runs test programs one after another, and most of ours start libobs and record
/// real seconds of video, so they spend their time waiting. This builds them all, then runs them
/// side by side, slowest first (timings from the last run), so the whole suite takes about as
/// long as its slowest program. Arguments after `test` are passed to every program (a filter,
/// `--nocapture`, ...). Doc tests aren't run; `cargo test --doc` does those.
fn test(args: Vec<String>) -> anyhow::Result<()> {
    use std::io::BufRead;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let started = Instant::now();
    // Polite: the build and the tests run at a lower priority, so the Mac stays responsive
    // (typing elsewhere keeps up) while they run.
    unsafe { setpriority(0, 0, 10) };
    let mut build = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["test", "--workspace", "--no-run", "--message-format=json-render-diagnostics"])
        .current_dir(root())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let mut bins = Vec::new();
    for line in std::io::BufReader::new(build.stdout.take().unwrap()).lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line?) else { continue };
        if msg["reason"] != "compiler-artifact" || msg["profile"]["test"] != true {
            continue;
        }
        let Some(exe) = msg["executable"].as_str() else { continue };
        let manifest = PathBuf::from(msg["manifest_path"].as_str().unwrap_or_default());
        let package = manifest.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned());
        let target = msg["target"]["name"].as_str().unwrap_or_default();
        let kind = msg["target"]["kind"][0].as_str().unwrap_or_default();
        let name = match kind {
            "test" => format!("{}/{target}", package.unwrap_or_default()),
            _ => format!("{} ({kind})", package.unwrap_or_default()),
        };
        bins.push(TestBin { name, exe: exe.into(), dir: manifest.parent().unwrap().to_path_buf() });
    }
    anyhow::ensure!(build.wait()?.success(), "build failed");
    let built = started.elapsed();

    // Slowest first, so the long ones aren't left starting last.
    let times_path = root().join("target/test-times.json");
    let mut times: serde_json::Map<String, serde_json::Value> =
        std::fs::read_to_string(&times_path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    bins.sort_by(|a, b| {
        let t = |n: &str| times.get(n).and_then(|v| v.as_f64()).unwrap_or(f64::MAX);
        t(&b.name).total_cmp(&t(&a.name))
    });

    // Each program runs from a folder of its own (a hard link, no copy). macOS treats a bare
    // program's folder as its bundle and lists it while libobs starts; target/debug/deps holds
    // every build's leftovers (hundreds of thousands of files), which costs seconds per program.
    // The recording tests need OBS's mux helper next to the program.
    let run_dir = root().join("target/test-run");
    let mux = PathBuf::from(std::env::var("OBS_APP").unwrap_or_else(|_| "/Applications/OBS.app".into()))
        .join("Contents/MacOS/obs-ffmpeg-mux");
    for bin in &mut bins {
        let file = bin.exe.file_name().unwrap().to_owned();
        let dir = run_dir.join(&file);
        std::fs::create_dir_all(&dir)?;
        let linked = dir.join(&file);
        let _ = std::fs::remove_file(&linked);
        if std::fs::hard_link(&bin.exe, &linked).is_err() {
            std::fs::copy(&bin.exe, &linked)?;
        }
        let helper = dir.join("obs-ffmpeg-mux");
        if !helper.exists() && mux.exists() {
            let _ = std::os::unix::fs::symlink(&mux, &helper);
        }
        bin.exe = linked;
    }

    // Tests that sample frames mid-animation need the machine to themselves: they run after
    // the rest, one at a time. (Tests that count dropped frames skip that check while sharing.)
    const ALONE: &[&str] = &["lightair-preview/transitions"];
    let (alone, together): (Vec<TestBin>, Vec<TestBin>) =
        bins.into_iter().partition(|b| ALONE.contains(&b.name.as_str()));

    // half the cores: each program runs libobs and real-time audio threads of its own
    let jobs = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).div_ceil(2).max(2);
    let results = Arc::new(Mutex::new(Vec::new()));
    run_tests(together, jobs, &args, &results);
    run_tests(alone, 1, &args, &results);

    let results = std::mem::take(&mut *results.lock().unwrap());
    for (name, _, took, _, _) in &results {
        times.insert(name.clone(), serde_json::json!(took.as_secs_f64()));
    }
    let _ = std::fs::write(&times_path, serde_json::to_string_pretty(&times)?);
    let failed: Vec<_> = results.iter().filter(|r| !r.1).collect();
    for (name, _, _, _, log) in &failed {
        eprintln!("\n──── {name} ────\n{log}");
    }
    let passed: usize = results.iter().map(|r| r.3).sum();
    let serial: Duration = results.iter().map(|r| r.2).sum();
    eprintln!(
        "\n{} programs, {passed} tests, {} failed · build {:.1}s, run {:.1}s (one after another: {:.1}s)",
        results.len(),
        failed.len(),
        built.as_secs_f64(),
        (started.elapsed() - built).as_secs_f64(),
        serial.as_secs_f64(),
    );
    anyhow::ensure!(failed.is_empty(), "{} test program(s) failed", failed.len());
    Ok(())
}

/// A test program's result: name, passed, time, tests passed, output.
type TestResult = (String, bool, std::time::Duration, usize, String);

/// Runs `bins` on `jobs` threads, adding to `results`.
fn run_tests(
    bins: Vec<TestBin>,
    jobs: usize,
    args: &[String],
    results: &std::sync::Arc<std::sync::Mutex<Vec<TestResult>>>,
) {
    use std::sync::{Arc, Mutex};
    use std::time::Instant;
    let queue = Arc::new(Mutex::new(bins.into_iter().collect::<std::collections::VecDeque<_>>()));
    let workers: Vec<_> = (0..jobs)
        .map(|_| {
            let (queue, results, args) = (queue.clone(), results.clone(), args.to_vec());
            std::thread::spawn(move || {
                loop {
                    // Take the next one and let go of the lock before running it.
                    let next = queue.lock().unwrap().pop_front();
                    let Some(bin) = next else { break };
                    let t = Instant::now();
                    let out = Command::new(&bin.exe)
                        .args(&args)
                        .current_dir(&bin.dir)
                        .env("CARGO_MANIFEST_DIR", &bin.dir)
                        // Tests that measure frame timing skip that when sharing the GPU.
                        .envs((jobs > 1).then_some(("LIGHTAIR_TESTS_SHARE_GPU", "1")))
                        // Camera and capture-card modules wait seconds on system services as
                        // they load; tests don't use them (the smoke test asks for every module).
                        .env(
                            "LIGHTAIR_SKIP_MODULES",
                            std::env::var("LIGHTAIR_SKIP_MODULES").unwrap_or_else(|_| {
                                "mac-avcapture,mac-avcapture-legacy,mac-syphon,aja,decklink,vlc-video".into()
                            }),
                        )
                        .output();
                    let took = t.elapsed();
                    let (ok, log) = match out {
                        Ok(o) => (
                            o.status.success(),
                            format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)),
                        ),
                        Err(e) => (false, e.to_string()),
                    };
                    let passed: usize = log
                        .lines()
                        .filter_map(|l| l.strip_prefix("test result: "))
                        .filter_map(|l| l.split("; ").next()?.split_whitespace().nth(1)?.parse::<usize>().ok())
                        .sum();
                    eprintln!(
                        "{} {:>6.1}s  {} ({passed})",
                        if ok { "ok  " } else { "FAIL" },
                        took.as_secs_f64(),
                        bin.name
                    );
                    results.lock().unwrap().push((bin.name, ok, took, passed, log));
                }
            })
        })
        .collect();
    for w in workers {
        let _ = w.join();
    }
}

unsafe extern "C" {
    /// POSIX: (PRIO_PROCESS = 0, this process = 0, niceness). Children inherit it.
    fn setpriority(which: i32, who: u32, prio: i32) -> i32;
}
