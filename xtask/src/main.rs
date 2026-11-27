//! Build orchestration (docs/15-platform-distribution-licensing.md, "Building").
//!
//!   cargo xtask modules    configure and build every in-tree native module (modules/*: the
//!                              Effect Stack's trackers)
//!   cargo xtask bundle     assemble target/debug/Lightair.app for development (macOS)
//!   cargo xtask bundle --release   the same from an optimised build: target/release/Lightair.app
//!                              (for measuring performance)
//!   cargo xtask dist       the app to ship: target/dist/Lightair.app and Lightair-<version>.dmg,
//!                              everything it runs inside it, signed with a Developer ID and
//!                              notarized (LIGHTAIR_NOTARY_PROFILE) (macOS); on Windows
//!                              target/dist/Lightair and Lightair-<version>-windows-x64.zip,
//!                              everything beside lightair.exe (LIGHTAIR_SIGN_CERT signs it)
//!   cargo xtask dev        Windows: Chromium's runtime next to target/debug for `cargo run`
//!   cargo xtask virtual-camera [--uninstall]  build "Lightair Virtual Camera" (the camera
//!                          extension and its installer, modules/lightair-virtual-camera),
//!                          put the installer in /Applications and open it (macOS)
//!
//!   cargo xtask test [--machine] [filter]  build every test program, then run them all at once
//!
//! The edit loop (docs/build-speed.md):
//!   cargo xtask check [--changed]  `cargo check` every target for this Mac (`--changed`: only
//!                              the crates changed since the last commit and those that use them)
//!   cargo xtask precommit      clippy for macOS and for Windows, then `cargo xtask test`: what a
//!                              commit needs (0 warnings, 0 failures)
//!   cargo xtask prune          delete object files no program in target/debug/deps uses
//!   cargo xtask tidy [--dry-run]  free space in target/: every incremental directory, and all
//!                              but the newest 3 hashed copies of each crate (--dry-run lists
//!                              them; refuses while a cargo build holds the lock)

use std::path::{Path, PathBuf};

#[cfg(windows)]
use anyhow::Context;
use std::process::Command;

#[cfg(target_os = "macos")]
mod dist;
mod rules;
mod tidy;
mod update;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf()
}

/// Links `to` at `link` (Windows: copies it; symbolic links need developer mode there).
fn link_file(to: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    return std::os::unix::fs::symlink(to, link);
    #[cfg(not(unix))]
    return std::fs::copy(to, link).map(|_| ());
}


/// Lower priority for this process and its children, so the machine stays responsive.
fn be_polite() {
    #[cfg(unix)]
    unsafe {
        setpriority(0, 0, 10)
    };
}

fn run(cmd: &mut Command) -> anyhow::Result<()> {
    eprintln!("$ {cmd:?}");
    let status = cmd.status()?;
    anyhow::ensure!(status.success(), "command failed: {status}");
    Ok(())
}

/// The Stream Deck plugin (streamdeck/), packed as the file Stream Deck installs when opened:
/// a zip of the `.sdPlugin` folder.
pub(crate) fn stream_deck_plugin(dest: &Path) -> anyhow::Result<()> {
    let _ = std::fs::remove_file(dest);
    run(Command::new("/usr/bin/ditto").args(["-c", "-k", "--keepParent", "--norsrc", "--noextattr", "--noqtn"]).arg(root().join("streamdeck/app.lightair.sdPlugin")).arg(dest))
}

/// Where the CEF crate keeps its Chromium build: CEF_PATH (CI keeps it outside target/, cached on
/// its own), else target/cef (.cargo/config.toml).
fn cef_dir() -> PathBuf {
    std::env::var_os("CEF_PATH").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| root().join("target/cef"))
}

/// The publisher's private release settings, from `release.env` at the repository's root (kept
/// out of git: .gitignore): `KEY=value` lines, set for the build unless already set. Lightair's
/// registered apps (LIGHTAIR_YOUTUBE_CLIENT_ID, …: compiled in, docs/official-apis.md) and
/// LIGHTAIR_NOTARY_PROFILE. Read by `dist`, which builds on macOS.
#[cfg(target_os = "macos")]
pub(crate) fn release_env() {
    let Ok(text) = std::fs::read_to_string(root().join("release.env")) else { return };
    let mut set = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        let Some((k, v)) = line.split_once('=') else { continue };
        let (k, v) = (k.trim(), v.trim().trim_matches('"'));
        if v.is_empty() || std::env::var_os(k).is_some() {
            continue;
        }
        // SAFETY: set before any thread starts
        unsafe { std::env::set_var(k, v) };
        set.push(k.to_string());
    }
    if !set.is_empty() {
        eprintln!("release.env: {}", set.join(", "));
    }
}

/// The speech helper for voice-phrase automations (tools/speech, Swift: Apple's speech
/// recognition), built once into target/ and copied to `dest`. macOS only; without Swift it's
/// left out (voice phrases then say so).
pub(crate) fn speech_helper(dest: &Path) -> anyhow::Result<()> {
    if !cfg!(target_os = "macos") {
        return Ok(());
    }
    let src = root().join("tools/speech/main.swift");
    let built = root().join("target/lightair-speech");
    let stale = match (std::fs::metadata(&src).and_then(|m| m.modified()), std::fs::metadata(&built).and_then(|m| m.modified())) {
        (Ok(s), Ok(b)) => s > b,
        _ => true,
    };
    if stale {
        let ok = Command::new("swiftc").args(["-O", "-suppress-warnings"]).arg(&src).arg("-o").arg(&built).status().is_ok_and(|s| s.success());
        if !ok {
            eprintln!("left out the speech helper (no Swift compiler): voice-phrase automations won't hear");
            return Ok(());
        }
    }
    std::fs::copy(&built, dest)?;
    Ok(())
}

/// PiAir, the AI co-host (the private piair repo beside this one): its Bun-compiled binary,
/// built if needed (`bun scripts/build.ts`), copied to `dest`. Left out without the checkout or
/// Bun (the PiAir panel then says it isn't in this build).
pub(crate) fn piair_binary(dest: &Path) -> anyhow::Result<()> {
    let checkout = root().join("../piair");
    let (target, exe) = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => ("darwin-arm64", "piair"),
        ("macos", _) => ("darwin-x64", "piair"),
        _ => ("windows-x64", "piair.exe"),
    };
    let built = checkout.join("dist").join(target).join(exe);
    let newest_source = std::fs::read_dir(checkout.join("src"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.metadata().ok()?.modified().ok())
        .max();
    let stale = match (newest_source, std::fs::metadata(&built).and_then(|m| m.modified())) {
        (Some(s), Ok(b)) => s > b,
        (None, Ok(_)) => false,
        _ => true,
    };
    if stale && checkout.join("scripts/build.ts").exists() {
        let ok = Command::new("bun").arg("scripts/build.ts").arg(target).current_dir(&checkout).status().is_ok_and(|s| s.success());
        if !ok {
            eprintln!("couldn't build PiAir (bun scripts/build.ts {target} in ../piair)");
        }
    }
    if !built.exists() {
        eprintln!("left out PiAir (no ../piair/dist/{target}/{exe})");
        return Ok(());
    }
    std::fs::copy(&built, dest)?;
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
        let mut configure = Command::new("cmake");
        configure.arg("-S").arg(&dir).arg("-B").arg(&build);
        // the trackers alone: nothing of the Effect Stack's old libobs module
        configure.arg("-DLIGHTAIR_WITH_LIBOBS=OFF");
        if cfg!(windows) {
            // Visual Studio's generator (Ninja needs a developer prompt for MSVC)
            configure.args(["-A", "x64"]);
        } else {
            configure.args(["-G", "Ninja", "-DCMAKE_BUILD_TYPE=RelWithDebInfo", "-DCMAKE_OSX_ARCHITECTURES=arm64"]);
        }
        run(&mut configure)?;
        run(Command::new("cmake").arg("--build").arg(&build).args(["--config", "RelWithDebInfo"]))?;
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
	<key>NSCameraUseContinuityCameraDeviceType</key><true/>
	<key>NSMicrophoneUsageDescription</key><string>Lightair records and streams your microphone.</string>
	<key>NSAudioCaptureUsageDescription</key><string>Lightair records and streams the sound your Mac and its apps play.</string>
	<key>NSBluetoothAlwaysUsageDescription</key><string>Signing in to a platform with a passkey on your phone uses Bluetooth to reach it.</string>
	<key>NSLocationUsageDescription</key><string>A web page in Lightair asked where you are.</string>
	<key>NSLocationWhenInUseUsageDescription</key><string>A web page in Lightair asked where you are.</string>
	<key>NSSpeechRecognitionUsageDescription</key><string>Lightair listens for the phrases your automations run on (on this Mac where the language allows), and web pages in Lightair may ask to recognise speech.</string>
	<key>NSAppleEventsUsageDescription</key><string>Lightair asks Spotify and Music what's playing, for the Now playing widget.</string>
	<key>NSLocalNetworkUsageDescription</key><string>Lightair finds network video sources and serves the remote control API.</string>
</dict>
</plist>
"#;

/// Development bundle: the debug binary inside a real .app (so macOS asks Lightair, not the
/// terminal, for camera and microphone access), with Chromium linked in
/// (docs/15-platform-distribution-licensing.md, "Building").
fn bundle(release: bool) -> anyhow::Result<()> {
    anyhow::ensure!(cfg!(target_os = "macos"), "the .app bundle is for macOS; on Windows, `cargo xtask dev`");
    let root = root();
    let profile = if release { "release" } else { "debug" };
    let mut build = Command::new("cargo");
    build.current_dir(&root).args(["build", "-p", "lightair-app", "-p", "lightair-web-helper"]);
    if release {
        build.arg("--release");
    }
    run(&mut build)?;
    let app = root.join(format!("target/{profile}/Lightair.app/Contents"));
    let _ = std::fs::remove_dir_all(app.parent().unwrap());
    for dir in ["MacOS", "Frameworks", "PlugIns", "Resources"] {
        std::fs::create_dir_all(app.join(dir))?;
    }
    std::fs::copy(root.join(format!("target/{profile}/lightair")), app.join("MacOS/lightair"))?;
    std::fs::write(app.join("Info.plist"), INFO_PLIST)?;
    let icns = root.join("branding/final/icon/Lightair.icns");
    if icns.exists() {
        std::fs::copy(icns, app.join("Resources/Lightair.icns"))?;
    }
    stream_deck_plugin(&app.join("Resources/Lightair.streamDeckPlugin"))?;
    speech_helper(&app.join("MacOS/lightair-speech"))?;
    piair_binary(&app.join("MacOS/piair"))?;
    bundle_chromium(&root, &app, profile, false, "app.lightair.dev")?;
    // Sign the bundle itself so macOS privacy settings identify Lightair. Nested code keeps its
    // own signatures; nothing outside target/ is modified.
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

/// Lightair's Chromium for web page sources (lightair-web): the pinned CEF framework the cef
/// crate fetched into target/cef, and Chromium's four helper apps, Lightair's own (each the
/// lightair-web-helper binary under the name Chromium looks for).
/// `copy`: the framework copied in (the shipped app) rather than linked; `id`: the app's bundle
/// identifier, the helpers' prefix.
fn bundle_chromium(root: &Path, app: &Path, profile: &str, copy: bool, id: &str) -> anyhow::Result<()> {
    let framework = std::fs::read_dir(cef_dir())
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|version| std::fs::read_dir(version.path()).into_iter().flatten().flatten())
        .map(|platform| platform.path().join("Chromium Embedded Framework.framework"))
        .find(|p| p.exists());
    let Some(framework) = framework else {
        anyhow::ensure!(!copy, "no Chromium in target/cef (build lightair-web once)");
        eprintln!("no Chromium in target/cef (build lightair-web once): web pages won't paint");
        return Ok(());
    };
    let into = app.join("Frameworks/Chromium Embedded Framework.framework");
    if copy {
        run(Command::new("ditto").arg(&framework).arg(&into))?;
    } else {
        link_file(&framework, &into)?;
    }
    let prefix = id;
    let helper = root.join(format!("target/{profile}/lightair-web-helper"));
    for (suffix, id) in [("", ""), (" (GPU)", ".gpu"), (" (Renderer)", ".renderer"), (" (Plugin)", ".plugin")] {
        let name = format!("Lightair Helper{suffix}");
        let contents = app.join("Frameworks").join(format!("{name}.app")).join("Contents");
        std::fs::create_dir_all(contents.join("MacOS"))?;
        std::fs::copy(&helper, contents.join("MacOS").join(&name))?;
        std::fs::write(
            contents.join("Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleName</key><string>{name}</string>
	<key>CFBundleExecutable</key><string>{name}</string>
	<key>CFBundleIdentifier</key><string>{prefix}.helper{id}</string>
	<key>CFBundlePackageType</key><string>APPL</string>
	<key>CFBundleShortVersionString</key><string>0.1.0</string>
	<key>LSMinimumSystemVersion</key><string>14.0</string>
	<key>LSUIElement</key><true/>
	<key>NSSupportsAutomaticGraphicsSwitching</key><true/>
</dict>
</plist>
"#
            ),
        )?;
    }
    Ok(())
}

/// Windows: Chromium's runtime next to the debug binary (lightair.exe loads libcef.dll as it
/// starts, and Chromium finds its resources beside it), for `target/debug`, or `target/release`
/// with `--release`. macOS needs nothing beside it.
fn dev(release: bool) -> anyhow::Result<()> {
    if !cfg!(windows) {
        eprintln!("nothing to put beside the binary on macOS (`cargo xtask bundle` makes the app)");
        return Ok(());
    }
    let profile = if release { "release" } else { "debug" };
    let out = root().join("target").join(profile);
    for dir in [out.clone(), out.join("deps")] {
        std::fs::create_dir_all(&dir)?;
        chromium_runtime(&dir)?;
    }
    Ok(())
}

/// Windows: the app to ship, `target/dist/Lightair` (a folder that runs from anywhere) and
/// `target/dist/Lightair-<version>-windows-x64.zip`. Everything Lightair runs sits beside
/// `lightair.exe`: Chromium for web sources, the MSVC runtime, the Effect Stack's trackers
/// (`lightair-tracker.dll` with ONNX Runtime and their models; DirectML is the system's), the virtual camera's
/// `lightair_camera.dll`, the command-line tools some features run (FFmpeg; whisper.cpp when
/// it's installed) and PiAir. Nothing of libobs or OBS is in it. From it, the installers: the
/// classic one (`packaging/windows/lightair.iss`, when Inno Setup is installed) and the MSIX
/// (`packaging/windows/AppxManifest.xml`, with the Windows SDK's makeappx).
///
/// Signed with Authenticode (`signtool`) when `LIGHTAIR_SIGN_CERT` names a certificate's SHA-1
/// thumbprint in the user's store; unsigned otherwise (SmartScreen warns on first run).
#[cfg(windows)]
fn dist_windows() -> anyhow::Result<()> {
    let root = root();
    run(Command::new("cargo").current_dir(&root).args([
        "build",
        "--release",
        "-p",
        "lightair-app",
        "-p",
        "lightair-web-helper",
        "-p",
        "lightair-camera-win",
    ]))?;
    modules()?;
    let dist = root.join("target/dist");
    let out = dist.join("Lightair");
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out)?;
    let release = root.join("target/release");
    for file in ["lightair.exe", "lightair-web-helper.exe", "lightair_camera.dll"] {
        std::fs::copy(release.join(file), out.join(file)).with_context(|| file.to_string())?;
    }
    chromium_runtime(&out)?;
    // the MSVC runtime the binaries link
    let crt = msvc_runtime().ok_or_else(|| anyhow::anyhow!("no MSVC runtime (Visual Studio's redistributable)"))?;
    for entry in std::fs::read_dir(&crt)? {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == "dll") {
            std::fs::copy(&path, out.join(path.file_name().unwrap()))?;
        }
    }
    // the trackers and what they run on: ONNX Runtime (MIT); DirectML is Windows' own (System32,
    // Windows 10 1903 on), unless LIGHTAIR_BUNDLE_DIRECTML ships Microsoft's redistributable
    // beside it (its own license terms, not the GPL's: see docs/15-platform-distribution-licensing.md)
    let modules = root.join("target/modules/lightair-effect-stack");
    let mut trackers = vec!["lightair-tracker.dll", "onnxruntime.dll"];
    if std::env::var_os("LIGHTAIR_BUNDLE_DIRECTML").is_some() {
        trackers.push("DirectML.dll");
    }
    for file in trackers {
        std::fs::copy(modules.join(file), out.join(file)).with_context(|| file.to_string())?;
    }
    copy_dir(&modules.join("tracker-models"), &out.join("tracker-models"))?;
    // the tools features run (found beside the app first)
    for (tool, needed) in [("ffmpeg", true), ("ffprobe", true), ("whisper-cli", false)] {
        match installed_tool(tool) {
            Some(found) => {
                std::fs::copy(&found, out.join(format!("{tool}.exe")))?;
            }
            None if needed => anyhow::bail!("{tool} isn't installed (scoop install ffmpeg)"),
            None => eprintln!("{tool} isn't installed: transcription won't be in the app"),
        }
    }
    if let Err(e) = piair_binary(&out.join("piair.exe")) {
        eprintln!("PiAir isn't in the app: {e}");
    }
    // signed, when there's a certificate
    let signing = std::env::var("LIGHTAIR_SIGN_CERT").ok();
    if let Some(thumbprint) = &signing {
        for entry in std::fs::read_dir(&out)? {
            let path = entry?.path();
            let ours = path.file_name().is_some_and(|n| {
                let n = n.to_string_lossy();
                n.starts_with("lightair") || n == "piair.exe"
            });
            if ours {
                sign(thumbprint, &path)?;
            }
        }
    } else {
        eprintln!("unsigned (set LIGHTAIR_SIGN_CERT to a code-signing certificate's thumbprint)");
    }
    let version = app_version()?;
    let zip = dist.join(format!("Lightair-{version}-windows-x64.zip"));
    let _ = std::fs::remove_file(&zip);
    // (Windows' own tar writes zip files)
    run(Command::new("tar").arg("-a").arg("-c").arg("-f").arg(&zip).arg("-C").arg(&dist).arg("Lightair"))?;
    eprintln!("built {} and {}", out.display(), zip.display());
    // the installers: the classic one (Inno Setup) and the MSIX
    match installed_tool("iscc") {
        Some(iscc) => {
            let setup = dist.join(format!("Lightair-{version}-windows-x64-setup.exe"));
            let _ = std::fs::remove_file(&setup);
            run(Command::new(iscc)
                .current_dir(root.join("packaging/windows"))
                .arg("/Q")
                .arg(format!("/DVersion={version}"))
                .arg(format!("/DSource={}", out.display()))
                .arg(format!("/DOutput={}", dist.display()))
                .arg("lightair.iss"))?;
            if let Some(thumbprint) = &signing {
                sign(thumbprint, &setup)?;
            }
            eprintln!("built {}", setup.display());
        }
        None => eprintln!("no installer: Inno Setup isn't installed (scoop install extras/inno-setup)"),
    }
    match windows_sdk_tool("makeappx.exe") {
        Some(makeappx) => {
            let msix = msix(&root, &out, &dist, &version, &makeappx)?;
            match &signing {
                Some(thumbprint) => sign(thumbprint, &msix)?,
                None => eprintln!("the MSIX installs only once signed (or registered unpacked in Developer Mode)"),
            }
            eprintln!("built {}", msix.display());
        }
        None => eprintln!("no MSIX: the Windows SDK's makeappx isn't installed"),
    }
    Ok(())
}

/// Signs a file with Authenticode (`signtool`), with the certificate of that SHA-1 thumbprint
/// in the user's store, timestamped.
#[cfg(windows)]
fn sign(thumbprint: &str, file: &Path) -> anyhow::Result<()> {
    let signtool = windows_sdk_tool("signtool.exe").unwrap_or_else(|| PathBuf::from("signtool"));
    run(Command::new(signtool)
        .args(["sign", "/fd", "SHA256", "/tr", "http://timestamp.digicert.com", "/td", "SHA256", "/sha1"])
        .arg(thumbprint)
        .arg(file))
}

/// A tool of the newest Windows SDK installed (`bin\<version>\x64`).
#[cfg(windows)]
fn windows_sdk_tool(name: &str) -> Option<PathBuf> {
    let bin = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut found: Vec<PathBuf> = std::fs::read_dir(bin)
        .into_iter()
        .flatten()
        .flatten()
        .map(|v| v.path().join("x64").join(name))
        .filter(|p| p.is_file())
        .collect();
    found.sort();
    found.pop()
}

/// The MSIX: `out` as it is (listed in a mapping file, not copied), with
/// `packaging/windows/AppxManifest.xml` (its version and publisher filled in) and its tiles.
#[cfg(windows)]
fn msix(root: &Path, out: &Path, dist: &Path, version: &str, makeappx: &Path) -> anyhow::Result<PathBuf> {
    let packaging = root.join("packaging/windows");
    // MSIX versions have four parts
    let mut parts: Vec<&str> = version.split(['.', '-', '+']).take(4).collect();
    parts.resize(4, "0");
    let publisher = std::env::var("LIGHTAIR_MSIX_PUBLISHER").unwrap_or_else(|_| "CN=Lightair Project".into());
    let manifest = std::fs::read_to_string(packaging.join("AppxManifest.xml"))?
        .replace("{VERSION}", &parts.join("."))
        .replace("{PUBLISHER}", &publisher);
    let filled = dist.join("AppxManifest.xml");
    std::fs::write(&filled, manifest)?;
    let mut map = String::from("[Files]\n");
    let mut line = |from: &Path, to: &str| map.push_str(&format!("\"{}\" \"{}\"\n", from.display(), to));
    line(&filled, "AppxManifest.xml");
    for entry in std::fs::read_dir(packaging.join("Assets"))? {
        let path = entry?.path();
        line(&path, &format!("Assets\\{}", path.file_name().unwrap().to_string_lossy()));
    }
    fn walk(dir: &Path, base: &Path, line: &mut dyn FnMut(&Path, &str)) -> anyhow::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            if path.is_dir() {
                walk(&path, base, line)?;
            } else {
                let rel = path.strip_prefix(base)?.to_string_lossy().replace('/', "\\");
                line(&path, &rel);
            }
        }
        Ok(())
    }
    walk(out, out, &mut line)?;
    let mapping = dist.join("msix.map");
    std::fs::write(&mapping, map)?;
    let msix = dist.join(format!("Lightair-{version}-windows-x64.msix"));
    run(Command::new(makeappx).args(["pack", "/o", "/f"]).arg(&mapping).arg("/p").arg(&msix))?;
    Ok(msix)
}

/// A command-line tool as installed: on PATH (through a Scoop shim, the program it points at).
#[cfg(windows)]
fn installed_tool(tool: &str) -> Option<PathBuf> {
    let exe = format!("{tool}.exe");
    let found = std::env::split_paths(&std::env::var_os("PATH")?).map(|d| d.join(&exe)).find(|p| p.is_file())?;
    let shim = found.with_extension("shim");
    if let Ok(text) = std::fs::read_to_string(&shim) {
        let target = text.lines().find_map(|l| l.trim().strip_prefix("path = "))?.trim().trim_matches('"');
        return Some(PathBuf::from(target));
    }
    Some(found)
}

/// The app's version (lightair-app's, from the workspace).
#[cfg(windows)]
fn app_version() -> anyhow::Result<String> {
    let text = std::fs::read_to_string(root().join("Cargo.toml"))?;
    let in_package = text.split("[workspace.package]").nth(1).unwrap_or("");
    let version = in_package
        .lines()
        .find_map(|l| l.trim().strip_prefix("version").map(|v| v.trim_start_matches([' ', '=']).trim().trim_matches('"').to_string()))
        .ok_or_else(|| anyhow::anyhow!("no version in the workspace's Cargo.toml"))?;
    Ok(version)
}

/// Copies a folder's files and folders into `to`.
#[cfg(windows)]
fn copy_dir(from: &Path, to: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let dst = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &dst)?;
        } else {
            std::fs::copy(entry.path(), dst)?;
        }
    }
    Ok(())
}

/// The MSVC runtime's redistributable folder (`Microsoft.VC143.CRT`, x64) of an installed
/// Visual Studio or its Build Tools.
#[cfg(windows)]
fn msvc_runtime() -> Option<PathBuf> {
    let vs = PathBuf::from(r"C:\Program Files (x86)\Microsoft Visual Studio");
    let alt = PathBuf::from(r"C:\Program Files\Microsoft Visual Studio");
    let mut found: Vec<PathBuf> = Vec::new();
    for base in [vs, alt] {
        for year in std::fs::read_dir(&base).into_iter().flatten().flatten() {
            for edition in std::fs::read_dir(year.path()).into_iter().flatten().flatten() {
                let redist = edition.path().join(r"VC\Redist\MSVC");
                for version in std::fs::read_dir(&redist).into_iter().flatten().flatten() {
                    let crt = version.path().join(r"x64\Microsoft.VC143.CRT");
                    if crt.join("vcruntime140.dll").exists() {
                        found.push(crt);
                    }
                }
            }
        }
    }
    found.sort();
    found.pop()
}

/// Windows: copies Chromium's runtime (CEF's Release folder and its Resources, as fetched
/// into target/cef) into `dir`, where only what's changed is copied again.
fn chromium_runtime(dir: &Path) -> anyhow::Result<()> {
    let Some(cef) = std::fs::read_dir(cef_dir())
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|version| std::fs::read_dir(version.path()).into_iter().flatten().flatten())
        .map(|platform| platform.path())
        .find(|p| p.join("libcef.dll").exists())
    else {
        eprintln!("no Chromium in target/cef (build lightair-web once): web pages won't paint");
        return Ok(());
    };
    fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            let (src, dst) = (entry.path(), to.join(entry.file_name()));
            if entry.file_type()?.is_dir() {
                copy_tree(&src, &dst)?;
            } else {
                let same = std::fs::metadata(&dst).ok().zip(entry.metadata().ok()).is_some_and(|(d, s)| d.len() == s.len() && d.modified().ok() >= s.modified().ok());
                if !same {
                    std::fs::copy(&src, &dst)?;
                }
            }
        }
        Ok(())
    }
    for entry in std::fs::read_dir(&cef)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let wanted = name.ends_with(".dll") || name.ends_with(".pak") || name.ends_with(".dat") || name.ends_with(".bin") || name.ends_with(".json");
        if entry.file_type()?.is_dir() && name == "locales" {
            copy_tree(&entry.path(), &dir.join("locales"))?;
        } else if wanted && name != "archive.json" {
            let dst = dir.join(&name);
            let same = std::fs::metadata(&dst).ok().zip(entry.metadata().ok()).is_some_and(|(d, s)| d.len() == s.len());
            if !same {
                std::fs::copy(entry.path(), dst)?;
            }
        }
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("modules") => modules(),
        Some("core-pin") => core_pin(std::env::args().nth(2)),
        Some("core-link") => core_link(std::env::args().nth(2)),
        Some("core-unlink") => core_unlink(),
        Some("bundle") => bundle(std::env::args().any(|a| a == "--release")),
        Some("dev") => dev(std::env::args().any(|a| a == "--release")),
        Some("virtual-camera") => virtual_camera(std::env::args().any(|a| a == "--uninstall")),
        Some("test") => test(std::env::args().skip(2).collect()),
        Some("check") => check(std::env::args().any(|a| a == "--changed")),
        Some("precommit") => precommit(),
        #[cfg(target_os = "macos")]
        Some("dist") => dist::dist(),
        #[cfg(windows)]
        Some("dist") => dist_windows(),
        #[cfg(not(any(target_os = "macos", windows)))]
        Some("dist") => anyhow::bail!("the shipped app is built on macOS and Windows"),
        Some("rules-key") => rules::key(),
        Some("update-key") => update::key(),
        Some("update-manifest") => {
            let a: Vec<String> = std::env::args().skip(2).collect();
            anyhow::ensure!(a.len() >= 2, "usage: cargo xtask update-manifest <version> <folder> [notes]");
            update::manifest(&a[0], &a[1], a.get(2).map(String::as_str))
        }
        Some("ci-secrets") => update::ci_secrets(),
        Some("rules-sign") => rules::sign(&std::env::args().nth(2).ok_or_else(|| anyhow::anyhow!("usage: cargo xtask rules-sign <rules.json>"))?),
        Some("tidy") => tidy::tidy(&root(), std::env::args().any(|a| a == "--dry-run")).map_err(anyhow::Error::msg),
        Some("prune") => {
            be_polite();
            tidy::prune(&root()).map_err(anyhow::Error::msg)
        }
        _ => {
            eprintln!(
                "usage: cargo xtask (modules | bundle | dist | dev | virtual-camera [--uninstall] | test [--machine] [filter] | check [--changed] | precommit | prune | tidy [--dry-run] | rules-key | rules-sign <file> | update-key | update-manifest <version> <folder> [notes] | ci-secrets)"
            );
            Ok(())
        }
    }
}

fn cargo() -> Command {
    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.current_dir(root());
    cmd
}

/// Workspace packages: name, folder (relative to the root), and the workspace packages each one
/// uses (dev-dependencies too: `--all-targets` builds the tests).
fn packages() -> anyhow::Result<Vec<(String, PathBuf, Vec<String>)>> {
    let out = cargo().args(["metadata", "--format-version", "1", "--no-deps", "--offline"]).output()?;
    anyhow::ensure!(out.status.success(), "cargo metadata failed");
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let root = root();
    Ok(meta["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|p| {
            let manifest = PathBuf::from(p["manifest_path"].as_str().unwrap_or_default());
            let dir = manifest.parent().unwrap_or(&root).strip_prefix(&root).unwrap_or(Path::new("")).to_path_buf();
            let uses = p["dependencies"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|d| d["path"].is_string())
                .filter_map(|d| d["name"].as_str().map(str::to_string))
                .collect();
            (p["name"].as_str().unwrap_or_default().to_string(), dir, uses)
        })
        .collect())
}

/// The packages changed since the last commit (staged, unstaged or new files), and every package
/// that uses one of them; `None` when a change touches the whole workspace (Cargo.toml at the
/// root, Cargo.lock, .cargo, the toolchain).
fn changed_packages() -> anyhow::Result<Option<Vec<String>>> {
    let out = Command::new("git").current_dir(root()).args(["status", "--porcelain", "-uall"]).output()?;
    anyhow::ensure!(out.status.success(), "git status failed");
    let paths: Vec<PathBuf> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.get(3..))
        // a rename: "old -> new"; both changed
        .flat_map(|p| p.split(" -> ").map(|p| PathBuf::from(p.trim_matches('"'))).collect::<Vec<_>>())
        .collect();
    let packages = packages()?;
    let mut changed: Vec<String> = Vec::new();
    for path in &paths {
        // the deepest package folder holding it (the workspace root is "")
        let owner = packages
            .iter()
            .filter(|(_, dir, _)| !dir.as_os_str().is_empty() && path.starts_with(dir))
            .max_by_key(|(_, dir, _)| dir.components().count());
        match owner {
            Some((name, ..)) => changed.push(name.clone()),
            None if path.starts_with("docs") || path.extension().is_some_and(|e| e == "md") => {}
            None if path.starts_with("modules") || path.starts_with("branding") || path.starts_with("screenshots") => {}
            None => return Ok(None),
        }
    }
    // and whatever uses them, until nothing new is added
    loop {
        let more: Vec<String> = packages
            .iter()
            .filter(|(name, _, uses)| !changed.contains(name) && uses.iter().any(|u| changed.contains(u)))
            .map(|(name, ..)| name.clone())
            .collect();
        if more.is_empty() {
            break;
        }
        changed.extend(more);
    }
    changed.sort();
    changed.dedup();
    Ok(Some(changed))
}

// ── lightair-core: the shared crates, pinned to a commit ────────────────────────────────────

const CORE_URL: &str = "https://github.com/Lightair-Project/lightair-core";

fn core_checkout() -> PathBuf {
    root().join("../lightair-core")
}

/// The shared crates the workspace takes from lightair-core (every `git = CORE_URL` dependency).
fn core_crates() -> anyhow::Result<Vec<String>> {
    let text = std::fs::read_to_string(root().join("Cargo.toml"))?;
    Ok(text
        .lines()
        .filter(|l| l.contains(CORE_URL))
        .filter_map(|l| l.split_once(" = ").map(|(name, _)| name.trim().to_string()))
        .collect())
}

/// `cargo xtask core-pin [<rev>]`: pins every lightair-core crate to a commit (default: the
/// checkout's HEAD), which must already be on GitHub, so anyone can build the desktop.
fn core_pin(rev: Option<String>) -> anyhow::Result<()> {
    let checkout = core_checkout();
    let git = |args: &[&str]| -> anyhow::Result<String> {
        let out = Command::new("git").arg("-C").arg(&checkout).args(args).output()?;
        anyhow::ensure!(out.status.success(), "git {} failed in {}", args.join(" "), checkout.display());
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    let rev = git(&["rev-parse", rev.as_deref().unwrap_or("HEAD")])?;
    git(&["fetch", "-q", "origin"])?;
    let on_github = Command::new("git").arg("-C").arg(&checkout).args(["merge-base", "--is-ancestor", &rev, "origin/main"]).status()?.success();
    anyhow::ensure!(on_github, "{rev} isn't on lightair-core's GitHub main yet; push it first");
    let path = root().join("Cargo.toml");
    let text = std::fs::read_to_string(&path)?;
    let mut changed = 0;
    let out: Vec<String> = text
        .lines()
        .map(|l| {
            if l.contains(CORE_URL)
                && let Some(i) = l.find("rev = \"")
                && let Some(j) = l[i + 7..].find('"')
            {
                changed += 1;
                return format!("{}{}{}", &l[..i + 7], rev, &l[i + 7 + j..]);
            }
            l.to_string()
        })
        .collect();
    std::fs::write(&path, out.join("\n") + "\n")?;
    run(cargo().args(["update", "--workspace", "--quiet"]))?;
    eprintln!("{changed} lightair-core crates pinned to {}", &rev[..12]);
    Ok(())
}

fn core_link_file() -> PathBuf {
    root().join(".cargo/core-link.toml")
}

/// `cargo xtask core-link [<path>]`: builds against a lightair-core checkout (default: the one
/// beside this repo) instead of the pinned commit, through `.cargo/core-link.toml`, which
/// `.cargo/config.toml` includes and git ignores. Only Cargo.lock changes; don't commit it while
/// linked (`cargo xtask ship` in lightair-core pins and commits it).
fn core_link(path: Option<String>) -> anyhow::Result<()> {
    let checkout = path.map(PathBuf::from).unwrap_or_else(core_checkout).canonicalize()?;
    let mut text = format!(
        "# Written by `cargo xtask core-link`; removed by `cargo xtask core-unlink`. Never committed.\n[patch.\"{CORE_URL}\"]\n"
    );
    for c in core_crates()? {
        text.push_str(&format!("{c} = {{ path = {:?} }}\n", checkout.join("crates").join(&c).display().to_string()));
    }
    std::fs::write(core_link_file(), text)?;
    eprintln!("building against {} (`cargo xtask core-unlink` to go back to the pin)", checkout.display());
    Ok(())
}

/// `cargo xtask core-unlink`: back to the pinned commit.
fn core_unlink() -> anyhow::Result<()> {
    match std::fs::remove_file(core_link_file()) {
        Ok(()) => {
            // Cargo.lock back to the pinned commit's sources
            run(cargo().args(["metadata", "--format-version", "1", "--quiet"]).stdout(std::process::Stdio::null()))?;
            eprintln!("lightair-core link removed: the pinned commit again");
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

fn core_linked() -> bool {
    core_link_file().exists()
}

/// The Effect Stack's .effect files and lightair-render's copies of them (whose uniforms the
/// Editor's WGSL ports read) must match: the module here is their source.
fn effects_in_sync() -> anyhow::Result<()> {
    let ours = root().join("modules/lightair-effect-stack/data/fx");
    let theirs = root().join("../lightair-core/crates/lightair-render/fx");
    if !theirs.exists() {
        eprintln!("lightair-core isn't beside this repo; the .effect files weren't compared");
        return Ok(());
    }
    let mut differ = Vec::new();
    for entry in std::fs::read_dir(&ours)?.flatten() {
        let name = entry.file_name();
        if std::fs::read(entry.path()).ok() != std::fs::read(theirs.join(&name)).ok() {
            differ.push(name.to_string_lossy().into_owned());
        }
    }
    differ.sort();
    anyhow::ensure!(
        differ.is_empty(),
        "these .effect files differ from lightair-core's copies (crates/lightair-render/fx): {}; copy them there",
        differ.join(", ")
    );
    Ok(())
}

/// The inner loop: `cargo check` (all targets, so tests too) for this Mac; clippy and Windows
/// are for `precommit`. The whole workspace by default: crates that didn't change cost almost
/// nothing to check, while checking a subset (`--changed`) unifies dependencies' features over
/// that subset only, so each new subset first rebuilds some dependencies (docs/build-speed.md).
fn check(changed: bool) -> anyhow::Result<()> {
    be_polite();
    if core_linked() {
        eprintln!("note: lightair-core is linked to a local checkout, so this checks against it, not the pin");
    }
    effects_in_sync()?;
    // Plain `cargo build` and `cargo test` leave object files behind too: past 50,000 files in
    // the folder (a few seconds per crate again), clear them first.
    if tidy::deps_files(&root()) > tidy::CROWDED {
        tidy::prune(&root()).map_err(anyhow::Error::msg)?;
    }
    let mut cmd = cargo();
    cmd.args(["check", "--all-targets"]);
    match if changed { changed_packages()? } else { None } {
        None => {
            cmd.arg("--workspace");
        }
        Some(packages) if packages.is_empty() => {
            eprintln!("nothing changed since the last commit (`cargo xtask check` checks everything)");
            return Ok(());
        }
        Some(packages) => {
            for p in packages {
                cmd.args(["-p", &p]);
            }
        }
    }
    run(&mut cmd)
}

/// Runs clippy for the workspace and all targets, counting warnings (cached results are
/// replayed, so a clean build with nothing to redo still reports them).
fn clippy(target: Option<&str>) -> anyhow::Result<usize> {
    use std::io::BufRead;
    let mut cmd = cargo();
    cmd.args(["clippy", "--workspace", "--all-targets", "--message-format=json-render-diagnostics"]);
    if let Some(t) = target {
        cmd.args(["--target", t]);
        // checking Windows from a Mac: lightair-web leaves Chromium out (Windows CEF can't be
        // built here; on Windows itself it's built as on macOS)
        if t.contains("windows") && !cfg!(windows) {
            cmd.env("RUSTFLAGS", "--cfg lightair_cross_check");
        }
        // gpui's build script compiles a Windows resource file with llvm-rc; Homebrew's LLVM
        // keeps it off PATH.
        let path = std::env::var_os("PATH").unwrap_or_default();
        let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
        if !dirs.iter().any(|d| d.join("llvm-rc").exists())
            && let Some(llvm) = ["/opt/homebrew/opt/llvm/bin", "/usr/local/opt/llvm/bin"]
                .map(PathBuf::from)
                .into_iter()
                .find(|d| d.join("llvm-rc").exists())
        {
            dirs.push(llvm);
            cmd.env("PATH", std::env::join_paths(dirs)?);
        }
    }
    eprintln!(
        "$ cargo clippy --workspace --all-targets{}",
        target.map(|t| format!(" --target {t}")).unwrap_or_default()
    );
    let mut child = cmd.stdout(std::process::Stdio::piped()).spawn()?;
    let mut warnings = 0;
    for line in std::io::BufReader::new(child.stdout.take().unwrap()).lines() {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line?) else { continue };
        if msg["reason"] == "compiler-message" && msg["message"]["level"] == "warning" {
            // rustc's own "N warnings emitted" summary has no code and no spans
            if msg["message"]["spans"].as_array().is_some_and(|s| !s.is_empty()) {
                warnings += 1;
            }
        }
    }
    anyhow::ensure!(child.wait()?.success(), "clippy failed");
    Ok(warnings)
}

/// What a commit needs: clippy clean for macOS and for Windows, and every test passing.
fn precommit() -> anyhow::Result<()> {
    be_polite();
    let started = std::time::Instant::now();
    let mac = clippy(None)?;
    let mac_took = started.elapsed();
    let windows = clippy(Some("x86_64-pc-windows-msvc"))?;
    let windows_took = started.elapsed() - mac_took;
    eprintln!(
        "clippy: {mac} warning(s) for macOS ({:.1}s), {windows} for Windows ({:.1}s)",
        mac_took.as_secs_f64(),
        windows_took.as_secs_f64()
    );
    anyhow::ensure!(mac + windows == 0, "clippy warnings");
    // the Stream Deck plugin, between a stand-in Stream Deck and Lightair (needs Node)
    if Command::new("node").arg("--version").output().is_ok_and(|o| o.status.success()) {
        run(Command::new("node").arg(root().join("streamdeck/test/run.js")))?;
    } else {
        eprintln!("skipped the Stream Deck plugin's test: no node");
    }
    test(Vec::new())
}

/// Object files are left in target/debug/deps by every incremental build of a test program or
/// binary (macOS keeps them for their debug info, under new names each build), and nothing
/// deletes the old ones. rustc lists that folder each time it starts, so a few hundred thousand
/// of them cost seconds per crate. This deletes those no program there refers to; with
/// `programs`, only the leftovers of those programs (just built). Returns (files, bytes).
fn prune(programs: Option<&[PathBuf]>) -> anyhow::Result<(usize, u64)> {
    if !cfg!(target_os = "macos") {
        return Ok((0, 0));
    }
    let deps = root().join("target/debug/deps");
    let mut objects: Vec<String> = Vec::new();
    let mut found: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(&deps)? {
        let name = entry?.file_name().to_string_lossy().into_owned();
        if name.ends_with(".rcgu.o") {
            objects.push(name);
        } else if !name.contains('.') && name.rsplit_once('-').is_some_and(|(_, h)| h.len() == 16) {
            found.push(deps.join(&name));
        }
    }
    // a program's own objects are named after it: `name-hash.<unit>.<build>.rcgu.o`
    let only: Option<Vec<String>> = programs.map(|programs| {
        programs.iter().filter_map(|p| Some(format!("{}.", p.file_name()?.to_string_lossy()))).collect()
    });
    let programs = programs.map(<[PathBuf]>::to_vec).unwrap_or(found);
    let mut used = std::collections::HashSet::new();
    let lists = std::thread::scope(|s| {
        let jobs: Vec<_> = programs
            .chunks(programs.len().div_ceil(8).max(1))
            .map(|chunk| {
                s.spawn(move || {
                    let mut names = Vec::new();
                    for p in chunk {
                        let Ok(out) = Command::new("nm").arg("-ap").arg(p).output() else { continue };
                        for line in String::from_utf8_lossy(&out.stdout).lines() {
                            if let Some((_, path)) = line.split_once(" OSO ") {
                                names.push(path.rsplit('/').next().unwrap_or(path).to_string());
                            }
                        }
                    }
                    names
                })
            })
            .collect();
        jobs.into_iter().flat_map(|j| j.join().unwrap_or_default()).collect::<Vec<_>>()
    });
    used.extend(lists);
    let (mut files, mut bytes) = (0, 0);
    for name in objects {
        let theirs = only.as_ref().is_none_or(|only| only.iter().any(|p| name.starts_with(p.as_str())));
        if used.contains(&name) || !theirs {
            continue;
        }
        let path = deps.join(&name);
        let size = std::fs::metadata(&path).map_or(0, |m| m.len());
        if std::fs::remove_file(&path).is_ok() {
            files += 1;
            bytes += size;
        }
    }
    Ok((files, bytes))
}

/// The SHA-1 of the first valid "Apple Development" code signing identity, if any.
/// The team of the keychain's Apple Development certificate (its subject's OU).
fn development_team() -> Option<String> {
    let pem = Command::new("security").args(["find-certificate", "-c", "Apple Development", "-p"]).output().ok()?;
    let mut x509 = Command::new("openssl")
        .args(["x509", "-noout", "-subject"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    use std::io::Write;
    x509.stdin.take()?.write_all(&pem.stdout).ok()?;
    let out = x509.wait_with_output().ok()?;
    let subject = String::from_utf8_lossy(&out.stdout).into_owned();
    let ou = subject.split(['/', ',']).map(str::trim).find_map(|p| p.strip_prefix("OU=").or_else(|| p.strip_prefix("OU = ")))?;
    let team: String = ou.chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
    (team.len() == 10).then_some(team)
}

/// "Lightair Virtual Camera" (modules/lightair-virtual-camera): XcodeGen makes the Xcode project
/// in target/virtual-camera, xcodebuild builds and signs it for the team (automatic signing makes
/// the provisioning profiles the extension needs), and the installer goes to /Applications, the
/// only place macOS installs camera extensions from, then opens (with `--uninstall`, to remove
/// the camera). macOS asks the user to allow the extension in System Settings the first time.
fn virtual_camera(uninstall: bool) -> anyhow::Result<()> {
    anyhow::ensure!(cfg!(target_os = "macos"), "the virtual camera extension is for macOS");
    let root = root();
    let team = std::env::var("LIGHTAIR_TEAM").ok().or_else(development_team).ok_or_else(|| {
        anyhow::anyhow!("no Apple Development certificate in the keychain (or set LIGHTAIR_TEAM)")
    })?;
    let out = root.join("target/virtual-camera");
    std::fs::create_dir_all(&out)?;
    run(Command::new("xcodegen")
        .args(["generate", "--quiet", "--spec"])
        .arg(root.join("modules/lightair-virtual-camera/project.yml"))
        .arg("--project")
        .arg(&out)
        .arg("--project-root")
        .arg(root.join("modules/lightair-virtual-camera")))?;
    run(Command::new("xcodebuild")
        .arg("-project")
        .arg(out.join("LightairVirtualCamera.xcodeproj"))
        .args(["-scheme", "Installer", "-configuration", "Release", "-quiet", "-allowProvisioningUpdates", "-derivedDataPath"])
        .arg(out.join("build"))
        .arg(format!("DEVELOPMENT_TEAM={team}"))
        .arg(format!("LIGHTAIR_VC_SRC={}", root.join("modules/lightair-virtual-camera").display()))
        .arg("build"))?;
    let built = out.join("build/Build/Products/Release/Lightair Virtual Camera.app");
    let installed = PathBuf::from("/Applications/Lightair Virtual Camera.app");
    if installed.exists() {
        std::fs::remove_dir_all(&installed)?;
    }
    run(Command::new("ditto").arg(&built).arg(&installed))?;
    let mut open = Command::new("open");
    open.arg("-n").arg(&installed);
    if uninstall {
        open.args(["--args", "--uninstall"]);
    }
    run(&mut open)?;
    eprintln!("opened {}", installed.display());
    Ok(())
}

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

/// Ignored tests that measure rather than test (run by hand, optimised): `--machine` leaves
/// them out.
const MEASUREMENTS: &[&str] = &["podcast_graph_cost", "frames_per_second", "swizzle_cost"];

/// `cargo test` runs test programs one after another, and some of ours play real-time audio or
/// wait on real seconds, so they spend their time waiting. This builds them all, then runs them
/// side by side, slowest first (timings from the last run), so the whole suite takes about as
/// long as its slowest program. Arguments after `test` are passed to every program (a filter,
/// `--nocapture`, ...). `--machine` also runs the tests that depend on this Mac (its audio
/// devices, its settings), which are ignored otherwise. Doc tests aren't run; `cargo test --doc`
/// does those.
fn test(args: Vec<String>) -> anyhow::Result<()> {
    use std::io::BufRead;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    let started = Instant::now();
    let machine = args.iter().any(|a| a == "--machine");
    let mut args: Vec<String> = args.into_iter().filter(|a| a != "--machine").collect();
    // the filter (the first argument that isn't a flag)
    let filter = args.iter().position(|a| !a.starts_with('-')).map(|i| args.remove(i));
    if machine {
        args.push("--include-ignored".into());
        for m in MEASUREMENTS {
            args.extend(["--skip".into(), m.to_string()]);
        }
    }
    args.extend(filter);
    // Polite: the build and the tests run at a lower priority, so the Mac stays responsive
    // (typing elsewhere keeps up) while they run.
    be_polite();
    let mut build = cargo()
        .args(["test", "--workspace", "--no-run", "--message-format=json-render-diagnostics"])
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
    let built_programs: Vec<PathBuf> = bins.iter().map(|b| b.exe.clone()).collect();
    let pruning = std::thread::spawn(move || prune(Some(&built_programs)));

    // Slowest first, so the long ones aren't left starting last.
    let times_path = root().join("target/test-times.json");
    let mut times: serde_json::Map<String, serde_json::Value> =
        std::fs::read_to_string(&times_path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    bins.sort_by(|a, b| {
        let t = |n: &str| times.get(n).and_then(|v| v.as_f64()).unwrap_or(f64::MAX);
        t(&b.name).total_cmp(&t(&a.name))
    });

    // Each program runs from a folder of its own (a hard link, no copy): macOS treats a bare
    // program's folder as its bundle and may list it, and target/debug/deps holds thousands of
    // files (more when builds' leftovers pile up), which costs seconds per program.
    let run_dir = root().join("target/test-run");
    let mut linked_to = std::collections::HashMap::new();
    for bin in &mut bins {
        if let Some(linked) = linked_to.get(&bin.exe) {
            bin.exe = PathBuf::clone(linked);
            continue;
        }
        let file = bin.exe.file_name().unwrap().to_owned();
        let dir = run_dir.join(&file);
        std::fs::create_dir_all(&dir)?;
        let linked = dir.join(&file);
        let _ = std::fs::remove_file(&linked);
        if std::fs::hard_link(&bin.exe, &linked).is_err() {
            std::fs::copy(&bin.exe, &linked)?;
        }
        linked_to.insert(std::mem::replace(&mut bin.exe, linked.clone()), linked);
    }

    // half the cores: some programs run real-time audio threads of their own
    let jobs = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8).div_ceil(2).max(2);
    let results = Arc::new(Mutex::new(Vec::new()));
    run_tests(bins, jobs, &args, &results);
    if let Ok(Ok((files, bytes))) = pruning.join()
        && files > 0
    {
        eprintln!("deleted {files} object files earlier builds left ({:.0} MB)", bytes as f64 / 1e6);
    }

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
        "\n{} processes, {passed} tests, {} failed · build {:.1}s, run {:.1}s (one after another: {:.1}s)",
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

#[cfg(unix)]
unsafe extern "C" {
    /// POSIX: (PRIO_PROCESS = 0, this process = 0, niceness). Children inherit it.
    fn setpriority(which: i32, who: u32, prio: i32) -> i32;
}
