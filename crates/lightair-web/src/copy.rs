//! Copies of a page's shared frames (macOS). Chromium hands each frame over in an IOSurface
//! from a pool of its own, which it may paint into again once the paint call has returned; a
//! frame kept longer than that is copied on the GPU (a Metal blit) into an IOSurface of the
//! caller's. Nothing crosses to the CPU and nothing waits: [`Copy::finished`] says when the
//! copy is done.

use std::ffi::c_void;
use std::sync::OnceLock;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_io_surface::IOSurfaceRef;
use objc2_metal::{
    MTLBlitCommandEncoder, MTLCommandBuffer, MTLCommandBufferStatus, MTLCommandEncoder, MTLCommandQueue,
    MTLCreateSystemDefaultDevice, MTLDevice, MTLPixelFormat, MTLTexture, MTLTextureDescriptor, MTLTextureUsage,
};

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceGetWidth(surface: *mut c_void) -> usize;
    fn IOSurfaceGetHeight(surface: *mut c_void) -> usize;
    fn IOSurfaceGetPixelFormat(surface: *mut c_void) -> u32;
}

/// `'BGRA'`: 8-bit blue, green, red, alpha (CoreVideo's 32BGRA).
const BGRA: u32 = u32::from_be_bytes(*b"BGRA");

struct Gpu {
    device: Retained<ProtocolObject<dyn MTLDevice>>,
    queue: Retained<ProtocolObject<dyn MTLCommandQueue>>,
}

/// The system's default Metal device and one queue for every copy (made on first use).
fn gpu() -> Option<&'static Gpu> {
    static GPU: OnceLock<Option<Gpu>> = OnceLock::new();
    GPU.get_or_init(|| {
        let device = MTLCreateSystemDefaultDevice()?;
        let queue = device.newCommandQueue()?;
        Some(Gpu { device, queue })
    })
    .as_ref()
}

/// A BGRA IOSurface's size, or `None` for another format or an empty surface.
fn bgra_size(surface: *mut c_void) -> Option<(usize, usize)> {
    if surface.is_null() {
        return None;
    }
    // SAFETY: `surface` is a live IOSurfaceRef (the caller's promise).
    let (w, h, format) = unsafe { (IOSurfaceGetWidth(surface), IOSurfaceGetHeight(surface), IOSurfaceGetPixelFormat(surface)) };
    (format == BGRA && w > 0 && h > 0).then_some((w, h))
}

/// A Metal texture over a BGRA IOSurface's memory (the texture keeps the surface alive).
fn texture(gpu: &Gpu, surface: *mut c_void, usage: MTLTextureUsage) -> Option<Retained<ProtocolObject<dyn MTLTexture>>> {
    let (w, h) = bgra_size(surface)?;
    // SAFETY: a plain 2D BGRA descriptor.
    let desc = unsafe {
        MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(MTLPixelFormat::BGRA8Unorm, w, h, false)
    };
    desc.setUsage(usage);
    // SAFETY: `surface` is a live IOSurfaceRef (checked above to be BGRA of this size).
    let surface = unsafe { &*(surface as *const IOSurfaceRef) };
    gpu.device.newTextureWithDescriptor_iosurface_plane(&desc, surface, 0)
}

/// An IOSurface of the caller's that frames are copied into: BGRA, made once and used for many
/// copies.
pub struct Target {
    texture: Retained<ProtocolObject<dyn MTLTexture>>,
    size: (usize, usize),
}

// SAFETY: Metal textures may be used from any thread; a target is only read (its texture passed
// to a blit) once made.
unsafe impl Send for Target {}
unsafe impl Sync for Target {}

impl Target {
    /// A target over `surface` (an `IOSurfaceRef`, BGRA): `None` for another format, or without
    /// Metal.
    ///
    /// # Safety
    /// `surface` must be a live IOSurfaceRef.
    pub unsafe fn new(surface: *mut c_void) -> Option<Target> {
        let gpu = gpu()?;
        let size = bgra_size(surface)?;
        let texture = texture(gpu, surface, MTLTextureUsage::ShaderRead | MTLTextureUsage::RenderTarget)?;
        Some(Target { texture, size })
    }

    /// Its size in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.size.0 as u32, self.size.1 as u32)
    }
}

/// A copy under way on the GPU.
pub struct Copy {
    commands: Retained<ProtocolObject<dyn MTLCommandBuffer>>,
}

// SAFETY: once committed the command buffer is only asked its status, which Metal allows from
// any thread.
unsafe impl Send for Copy {}
unsafe impl Sync for Copy {}

impl Copy {
    /// `None` while the GPU is copying; then whether the copy was made.
    pub fn finished(&self) -> Option<bool> {
        match self.commands.status() {
            MTLCommandBufferStatus::Completed => Some(true),
            MTLCommandBufferStatus::Error => Some(false),
            _ => None,
        }
    }
}

/// Starts copying `from` (an `IOSurfaceRef`, BGRA, the size of `to`) into `to`. `None` when it
/// can't: another format or size, or no Metal. Keep `from` from being painted into until the
/// copy has finished (retained, and in use: Chromium leaves a surface in use alone).
///
/// # Safety
/// `from` must be a live IOSurfaceRef.
pub unsafe fn copy(from: *mut c_void, to: &Target) -> Option<Copy> {
    let gpu = gpu()?;
    if bgra_size(from)? != to.size {
        return None;
    }
    let source = texture(gpu, from, MTLTextureUsage::ShaderRead)?;
    let commands = gpu.queue.commandBuffer()?;
    let blit = commands.blitCommandEncoder()?;
    // SAFETY: both textures are BGRA8Unorm 2D of the same size with one level; the command
    // buffer retains them until it has run.
    unsafe { blit.copyFromTexture_toTexture(&source, &to.texture) };
    blit.endEncoding();
    commands.commit();
    Some(Copy { commands })
}
