//! Generates bindings for the pinned libobs (third_party/obs-studio) and links libobs.
//!
//! Linking: development builds link the `libobs.framework` of an installed OBS of the same
//! version (OBS_APP, default /Applications/OBS.app). Release builds will link the libobs built
//! from the submodule (IMPLEMENTATION.md, M13).

use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let root = manifest.join("../..").canonicalize().expect("workspace root");
    let libobs = root.join("third_party/obs-studio/libobs");
    let simde = root.join("third_party/simde");
    assert!(libobs.join("obs.h").exists(), "libobs headers missing: run `git submodule update --init --depth 1`");

    // libobs.h includes a CMake-generated obsconfig.h; provide the release values.
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let include_dir = out.join("include");
    fs::create_dir_all(&include_dir).unwrap();
    // Written only when it differs: bindgen asks cargo to watch every header it reads, so a fresh
    // timestamp here would rebuild this crate and everything above it on every build.
    let config = include_dir.join("obsconfig.h");
    let contents = "#pragma once\n#define OBS_RELEASE_CANDIDATE 0\n#define OBS_BETA 0\n";
    if fs::read_to_string(&config).ok().as_deref() != Some(contents) {
        fs::write(&config, contents).unwrap();
    }

    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-env-changed=OBS_APP");
    println!("cargo:rerun-if-env-changed=OBS_DIR");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    // Checking the Windows build from a Mac: there are no Windows SDK headers here, so the
    // headers are read as arm64 macOS (a `char*` va_list, as on Windows). Good enough to
    // type-check Windows code (`cargo check`); never for a real Windows build, which runs bindgen
    // natively (MSVC, LLVM).
    let cross_check = target_os == "windows" && !cfg!(windows);
    let mut builder = bindgen::Builder::default();
    if cross_check {
        println!("cargo:warning=libobs bindings read as arm64 macOS: for checking the Windows build only");
        builder = builder.clang_arg("--target=arm64-apple-darwin");
    }

    let bindings = builder
        .header(manifest.join("wrapper.h").to_string_lossy())
        .clang_arg(format!("-I{}", libobs.display()))
        .clang_arg(format!("-I{}", include_dir.display()))
        .clang_arg(format!("-I{}", simde.display()))
        .allowlist_function("(obs|gs|os|calldata|signal_handler|proc_handler|video_output|audio_output|bmalloc|bfree|bzalloc|bstrdup|blog|base_set_log_handler|base_get_log_handler|profiler|config|vec4)_.*")
        .allowlist_function("blog|bfree|bmalloc|bstrdup|base_set_log_handler|base_get_log_handler|bnum_allocs")
        .allowlist_type("(obs|gs|video|audio|speaker)_.*")
        .allowlist_type("(calldata|signal_handler|proc_handler|profiler_name_store|config)_t")
        .allowlist_var("(OBS|GS|LOG|MODULE|LIBOBS|VIDEO|AUDIO|MAX)_.*")
        .default_enum_style(bindgen::EnumVariation::Consts)
        .prepend_enum_name(false)
        .derive_default(true)
        .layout_tests(false)
        .generate_comments(false)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .generate()
        .expect("bindgen failed for libobs");
    bindings.write_to_file(out.join("bindings.rs")).expect("write bindings");

    match target_os.as_str() {
        "macos" => {
            let obs_app = env::var("OBS_APP").unwrap_or_else(|_| "/Applications/OBS.app".into());
            println!("cargo:rustc-link-search=framework={obs_app}/Contents/Frameworks");
            println!("cargo:rustc-link-lib=framework=libobs");
            println!("cargo:obs_app={obs_app}");
        }
        // Windows: libobs's import library from the submodule's build (or an OBS install of the
        // same version): `OBS_DIR\lib` or `OBS_DIR\bin\64bit` holding `obs.lib`.
        "windows" if !cross_check => {
            let obs_dir = env::var("OBS_DIR").unwrap_or_else(|_| r"C:\Program Files\obs-studio".into());
            for sub in ["lib", r"bin\64bit"] {
                println!("cargo:rustc-link-search=native={obs_dir}\\{sub}");
            }
            println!("cargo:rustc-link-lib=dylib=obs");
            println!("cargo:obs_dir={obs_dir}");
        }
        _ => {}
    }
}
