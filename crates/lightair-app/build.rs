//! Windows: Lightair's icon and version information in lightair.exe (Explorer, the taskbar,
//! Alt-Tab, the installer). GPUI embeds the manifest (per-monitor DPI awareness) itself.

fn main() {
    // Lightair's catalog root, when this build has one (plugins: catalogs.rs).
    println!("cargo:rerun-if-changed=catalog/root-1.json");
    println!("cargo::rustc-check-cfg=cfg(lightair_catalog_root)");
    if std::path::Path::new("catalog/root-1.json").exists() {
        println!("cargo:rustc-cfg=lightair_catalog_root");
    }
    println!("cargo:rerun-if-changed=windows/lightair.rc");
    println!("cargo:rerun-if-changed=../../branding/final/icon/Lightair.ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") && cfg!(windows) {
        let _ = embed_resource::compile("windows/lightair.rc", embed_resource::NONE);
    }
}
