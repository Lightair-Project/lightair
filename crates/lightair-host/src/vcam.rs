//! The virtual camera: on macOS the camera extension (modules/lightair-virtual-camera), sent the
//! compositor's frames by [`crate::vcam_mac::Sink`] (`own_outputs::start_virtual_camera`).

/// The error when no camera is installed (the UI offers to install one).
pub const NOT_INSTALLED: &str = "The virtual camera isn't installed on this PC.";

/// Which camera the virtual camera output sends to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Camera {
    /// Lightair Virtual Camera (macOS: the camera extension).
    Lightair,
    /// OBS Virtual Camera (Windows, where Lightair's can't be).
    Obs,
}

impl Camera {
    /// What other apps list it as.
    pub fn name(self) -> &'static str {
        match self {
            Camera::Lightair => "Lightair Virtual Camera",
            Camera::Obs => "OBS Virtual Camera",
        }
    }
}

/// The camera to use; `None`: none here.
pub fn camera() -> Option<Camera> {
    cfg!(target_os = "macos").then_some(Camera::Lightair)
}

/// Whether a camera is installed.
pub fn installed() -> bool {
    camera().is_some()
}

/// Whether Lightair's own camera could be installed here but isn't.
pub fn lightair_installable() -> bool {
    false
}

/// Installs a camera (it comes with Lightair on macOS).
pub fn install() -> Result<(), String> {
    Err("The virtual camera installs with Lightair on this system.".into())
}
