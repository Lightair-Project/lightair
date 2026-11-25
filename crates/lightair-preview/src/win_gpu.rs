//! Windows: shared textures opened on the compositor's wgpu device (Direct3D 12) by their NT
//! handles, as macOS's compositor opens IOSurfaces as Metal textures. What's shared this way:
//! Lightair's own pictures (`lightair_host::buffers`: the compositor's output frames, which the
//! window's preview and the encoders read on their Direct3D 11 devices), captured frames
//! (`lightair_capture::Texture`), and the encoders' input slots (`lightair_encode`).
//!
//! Each is a Direct3D 11 texture made with `D3D11_RESOURCE_MISC_SHARED_NTHANDLE` on the same
//! graphics adapter as the wgpu device ([`share_adapter`] puts Lightair's own there). A texture
//! opened here starts in Direct3D 12's common state, as wgpu takes an uninitialised texture.
//!
//! Shared fences order the GPU work across devices: a captured frame's fence is waited for
//! before the submit that reads it ([`wait_before_next_submit`]), and an encoder's fence is
//! signalled after the submit that draws its input slot ([`signal_after_next_submit`]).

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Weak};

use lightair_host::buffers::Buffer;
use parking_lot::Mutex;
use windows::Win32::Foundation::{HANDLE, LUID};
use windows::Win32::Graphics::Direct3D12::{ID3D12Fence, ID3D12Resource};

/// The graphics adapter of `device`, when it runs on Direct3D 12.
pub fn adapter_of(device: &wgpu::Device) -> Option<LUID> {
    // SAFETY: the raw device is only asked for its adapter.
    let hal = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }?;
    Some(unsafe { hal.raw_device().GetAdapterLuid() })
}

/// Makes Lightair's own pictures (`lightair_host::buffers`) and captured frames
/// (`lightair_capture`) on `device`'s adapter from now on, so the compositor can open them. False
/// when `device` doesn't run on Direct3D 12.
pub fn share_adapter(device: &wgpu::Device) -> bool {
    let Some(luid) = adapter_of(device) else { return false };
    lightair_host::buffers::set_adapter(Some(luid));
    lightair_capture::set_adapter(Some(luid));
    true
}

/// The shared texture `handle` (a 2D texture of `size` in `format`) as a texture on `device`,
/// for `usage`. The handle stays its owner's. `None` when `device` doesn't run on Direct3D 12
/// or can't open it (another adapter, or not a shared texture).
pub fn open(
    device: &wgpu::Device,
    handle: HANDLE,
    size: (u32, u32),
    format: wgpu::TextureFormat,
    usage: wgpu::TextureUsages,
) -> Option<wgpu::Texture> {
    // SAFETY: the raw device is only used to open the handle.
    let hal = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }?;
    let mut resource: Option<ID3D12Resource> = None;
    // SAFETY: `handle` is an NT handle to a shared resource, borrowed for the call.
    if let Err(e) = unsafe { hal.raw_device().OpenSharedHandle(handle, &mut resource) } {
        tracing::warn!(%e, "couldn't open a shared texture on the compositor's device");
        return None;
    }
    let resource = resource?;
    drop(hal);
    let extent = wgpu::Extent3d { width: size.0, height: size.1, depth_or_array_layers: 1 };
    // SAFETY: the resource is a 2D texture of `size` in `format`, one mip level, one sample; the
    // texture made from it holds its own reference.
    let hal_texture = unsafe {
        wgpu::hal::dx12::Device::texture_from_raw(resource, format, wgpu::TextureDimension::D2, extent, 1, 1)
    };
    // SAFETY: the descriptor matches the resource opened above.
    let texture = unsafe {
        device.create_texture_from_hal::<wgpu::hal::api::Dx12>(
            hal_texture,
            &wgpu::TextureDescriptor {
                label: Some("lightair-shared"),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            },
            wgpu::TextureUses::UNINITIALIZED,
        )
    };
    Some(texture)
}

/// One of Lightair's own pictures as a BGRA texture on `device`, drawn into or read.
pub fn open_buffer(device: &wgpu::Device, buffer: &Buffer, usage: wgpu::TextureUsages) -> Option<wgpu::Texture> {
    open(device, buffer.handle(), buffer.size(), wgpu::TextureFormat::Bgra8Unorm, usage)
}

/// One of Lightair's own pictures as a texture on `device` to sample or copy from, opened once
/// and kept while the buffer lives (rings reuse their buffers). The buffer must be held until
/// the GPU is done with the texture.
pub fn texture_of(device: &wgpu::Device, buffer: &Buffer) -> Option<wgpu::Texture> {
    type Opened = HashMap<usize, (Weak<lightair_host::buffers::Shared>, wgpu::Device, wgpu::Texture)>;
    static OPENED: LazyLock<Mutex<Opened>> = LazyLock::new(Default::default);
    // a buffer's address: kept for it by its weak reference, so never another buffer's
    let at = Arc::as_ptr(buffer) as usize;
    let mut opened = OPENED.lock();
    if let Some((_, d, t)) = opened.get(&at)
        && d == device
    {
        return Some(t.clone());
    }
    // the textures of buffers gone are let go
    opened.retain(|_, (b, _, _)| b.strong_count() > 0);
    let usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_SRC;
    let texture = open_buffer(device, buffer, usage)?;
    opened.insert(at, (Arc::downgrade(buffer), device.clone(), texture.clone()));
    Some(texture)
}

/// A new picture of `size` to draw into (an output frame): the buffer, and its texture on
/// `device` as a render target.
pub fn new_output(device: &wgpu::Device, size: (u32, u32)) -> Option<(Buffer, wgpu::Texture)> {
    let buffer = lightair_host::buffers::bgra_buffer(size.0, size.1)?;
    let texture = open_buffer(device, &buffer, wgpu::TextureUsages::RENDER_ATTACHMENT)?;
    Some((buffer, texture))
}

/// A shared fence opened on the compositor's device.
pub struct Fence(ID3D12Fence);

// Direct3D 12 fences are free-threaded.
unsafe impl Send for Fence {}
unsafe impl Sync for Fence {}

/// The shared fence `handle` (an NT handle, which stays its owner's) on `device`.
pub fn open_fence(device: &wgpu::Device, handle: HANDLE) -> Option<Fence> {
    // SAFETY: the raw device is only used to open the handle.
    let hal = unsafe { device.as_hal::<wgpu::hal::api::Dx12>() }?;
    let mut fence: Option<ID3D12Fence> = None;
    // SAFETY: `handle` is an NT handle to a shared fence, borrowed for the call.
    if let Err(e) = unsafe { hal.raw_device().OpenSharedHandle(handle, &mut fence) } {
        tracing::warn!(%e, "couldn't open a shared fence on the compositor's device");
        return None;
    }
    fence.map(Fence)
}

/// The next submit on `queue` waits on the GPU until `fence` reaches `value` before its work
/// (a captured frame's copy finished). False when `queue` doesn't run on Direct3D 12.
pub fn wait_before_next_submit(queue: &wgpu::Queue, fence: &Fence, value: u64) -> bool {
    // SAFETY: the raw queue is only given a wait for its next submit.
    let Some(hal) = (unsafe { queue.as_hal::<wgpu::hal::api::Dx12>() }) else { return false };
    hal.add_wait_fence(fence.0.clone(), value);
    true
}

/// The next submit on `queue` signals `fence` with `value` once its work is done (an encoder's
/// input slot drawn). False when `queue` doesn't run on Direct3D 12.
pub fn signal_after_next_submit(queue: &wgpu::Queue, fence: &Fence, value: u64) -> bool {
    // SAFETY: the raw queue is only given a signal for its next submit.
    let Some(hal) = (unsafe { queue.as_hal::<wgpu::hal::api::Dx12>() }) else { return false };
    hal.add_signal_fence(fence.0.clone(), value);
    true
}
