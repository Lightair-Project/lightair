//! Lightair's own pictures (`lightair_host::buffers::Buffer`) as the compositor's GPU takes them,
//! one small set of calls for each platform:
//!
//! - **macOS**: a BGRA IOSurface-backed pixel buffer, given to the renderer as it is
//!   (`Picture::Buffer`) and sampled as a Metal texture over the same memory.
//! - **Windows**: a BGRA Direct3D 11 texture with an NT shared handle, opened on the compositor's
//!   Direct3D 12 device (`crate::win_gpu`) and given to the renderer as a texture. A buffer is
//!   opened once and kept open while it lives (capture and output rings reuse theirs).
//!
//! And `not_yet` (Windows): what the compositor can't draw there yet says so in the log, once.

#[cfg(windows)]
use std::collections::HashSet;
#[cfg(windows)]
use std::sync::LazyLock;

use lightair_host::buffers::Buffer;
use lightair_render::scene::Shot;
#[cfg(windows)]
use parking_lot::Mutex;

/// Logs once that `what` isn't done on this platform yet (drawn as nothing, or without it).
#[cfg(windows)]
pub(crate) fn not_yet(what: &str) {
    static SAID: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(Default::default);
    let mut said = SAID.lock();
    if !said.contains(what) {
        said.insert(what.to_owned());
        tracing::warn!("not yet on this platform: {what}");
    }
}

/// `buffer`'s size in pixels.
#[cfg(target_os = "macos")]
pub(crate) fn size(buffer: &Buffer) -> (u32, u32) {
    (objc2_core_video::CVPixelBufferGetWidth(buffer) as u32, objc2_core_video::CVPixelBufferGetHeight(buffer) as u32)
}

/// `buffer`'s size in pixels.
#[cfg(windows)]
pub(crate) fn size(buffer: &Buffer) -> (u32, u32) {
    buffer.size()
}

/// `buffer` as a texture to sample on `device`. The buffer must be held until the GPU is done.
#[cfg(target_os = "macos")]
pub(crate) fn sampled(device: &wgpu::Device, buffer: &Buffer) -> Option<wgpu::Texture> {
    lightair_engine::gpu::apple::sampled(device, buffer)
}

/// `buffer` as a texture to sample on `device`. The buffer must be held until the GPU is done.
#[cfg(windows)]
pub(crate) fn sampled(device: &wgpu::Device, buffer: &Buffer) -> Option<wgpu::Texture> {
    crate::win_gpu::texture_of(device, buffer)
}

/// `buffer` as the renderer draws it (`premultiplied`: its colours are multiplied by alpha). The
/// buffer must be held until the GPU is done.
#[cfg(target_os = "macos")]
pub(crate) fn shot(buffer: &Buffer, premultiplied: bool) -> Option<Shot> {
    use lightair_render::media::Picture;
    use lightair_render::texture::PixelBuffer;
    let raw = objc2_core_foundation::CFRetained::into_raw(buffer.clone());
    // SAFETY: the reference made by `clone` is given up to the pixel buffer, which releases it.
    let b = unsafe { PixelBuffer::from_raw(raw.as_ptr().cast()) }?;
    Some(Shot { size: b.size(), picture: Picture::Buffer(b), premultiplied })
}

/// `buffer` as the renderer draws it (`premultiplied`: its colours are multiplied by alpha). The
/// buffer must be held until the GPU is done.
#[cfg(windows)]
pub(crate) fn shot(buffer: &Buffer, premultiplied: bool) -> Option<Shot> {
    use lightair_render::media::Picture;
    static DEVICE: LazyLock<Option<wgpu::Device>> =
        LazyLock::new(|| lightair_render::shared_gpu().ok().map(|g| g.device));
    let texture = sampled(DEVICE.as_ref()?, buffer)?;
    Some(Shot { size: buffer.size(), picture: Picture::Texture(texture), premultiplied })
}

/// Whether something besides its owner holds `buffer` (it mustn't be drawn into again yet).
#[cfg(target_os = "macos")]
pub(crate) fn held_elsewhere(buffer: &Buffer) -> bool {
    buffer.retain_count() > 1
}

/// Whether something besides its owner holds `buffer` (it mustn't be drawn into again yet).
#[cfg(windows)]
pub(crate) fn held_elsewhere(buffer: &Buffer) -> bool {
    std::sync::Arc::strong_count(buffer) > 1
}
