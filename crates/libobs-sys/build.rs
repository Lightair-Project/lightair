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
    assert!(
        libobs.join("obs.h").exists(),
        "libobs headers missing: run `git submodule update --init --depth 1`"
    );

    // libobs.h includes a CMake-generated obsconfig.h; provide the release values.
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let include_dir = out.join("include");
    fs::create_dir_all(&include_dir).unwrap();
    fs::write(
        include_dir.join("obsconfig.h"),
        "#pragma once\n#define OBS_RELEASE_CANDIDATE 0\n#define OBS_BETA 0\n",
    )
    .unwrap();

    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-env-changed=OBS_APP");

    let bindings = bindgen::Builder::default()
        .header(manifest.join("wrapper.h").to_string_lossy())
        .clang_arg(format!("-I{}", libobs.display()))
        .clang_arg(format!("-I{}", include_dir.display()))
        .clang_arg(format!("-I{}", simde.display()))
        .allowlist_function("(obs|gs|os|calldata|signal_handler|proc_handler|video_output|audio_output|bmalloc|bfree|bzalloc|bstrdup|blog|base_set_log_handler|base_get_log_handler|profiler|config|vec4)_.*")
        .allowlist_function("blog|bfree|bmalloc|bstrdup|base_set_log_handler|base_get_log_handler")
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
    bindings
        .write_to_file(out.join("bindings.rs"))
        .expect("write bindings");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let obs_app = env::var("OBS_APP").unwrap_or_else(|_| "/Applications/OBS.app".into());
        println!("cargo:rustc-link-search=framework={obs_app}/Contents/Frameworks");
        println!("cargo:rustc-link-lib=framework=libobs");
        println!("cargo:obs_app={obs_app}");
    }
}
