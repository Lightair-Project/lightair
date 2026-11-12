//! Build orchestration (docs/15-platform-distribution-licensing.md, "Building").
//!
//!   cargo xtask modules    configure and build every in-tree libobs module (modules/*)
//!   cargo xtask bundle     assemble target/debug/Lightair.app for development (macOS)
//!   cargo xtask dev        link OBS's helper programs next to target/debug for `cargo run`

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
    // permission again after each rebuild. Set LIGHTAIR_SIGN_IDENTITY to a code signing identity
    // (e.g. "Apple Development") to keep one stable identity and keep the permissions.
    let identity = std::env::var("LIGHTAIR_SIGN_IDENTITY").ok().filter(|s| !s.is_empty());
    let identity = identity.as_deref().unwrap_or("-");
    run(Command::new("codesign").args(["--force", "--sign", identity]).arg(app.parent().unwrap()))?;
    if identity == "-" {
        eprintln!(
            "signed ad hoc: macOS will ask for permissions again after rebuilds (set LIGHTAIR_SIGN_IDENTITY to keep them)"
        );
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
        _ => {
            eprintln!("usage: cargo xtask (modules | bundle | dev)");
            Ok(())
        }
    }
}
