//! Chromium's helper process (renderer, GPU, utility…) for Lightair's web pages: Chromium starts
//! it with `--type=…`; it runs until Chromium is done with it.

fn main() {
    std::process::exit(lightair_web::helper_main().unwrap_or(0));
}
