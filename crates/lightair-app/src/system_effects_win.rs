//! The OS's own camera effects on Windows: Windows Studio Effects, where the camera and PC have
//! them (docs/plans/windows.md, phase 3). Until then nothing reports them on, and the button
//! opens the camera settings, where they live.

/// Opens Settings › Camera.
pub fn open_video_effects() {
    let _ = std::process::Command::new("explorer.exe").arg("ms-settings:camera").spawn();
}

/// Whether background blur is on: not known yet.
pub fn portrait_on() -> Option<bool> {
    None
}

/// Whether background replacement is on: Windows has none.
pub fn backgrounds_on() -> Option<bool> {
    None
}
