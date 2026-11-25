//! Windows: what Lightair's pictures travel in between the compositor, the previews and the
//! encoders, as IOSurface-backed pixel buffers do on macOS (`buffers.rs`): BGRA Direct3D 11
//! textures with NT shared handles, on one Direct3D 11 device per graphics adapter.
//!
//! The compositor (wgpu on Direct3D 12) opens a buffer by its handle and draws into it; the
//! window's preview copies it into its swap chain on the buffer's own device ([`Shared::device`]),
//! and Media Foundation's encoders take it the same way. Whoever draws into a buffer has finished
//! with it on the GPU before handing it on, so readers need no fence. Nothing here goes through
//! memory except [`from_rgba`] (pictures made on the CPU).

use std::sync::Arc;

use parking_lot::Mutex;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_RESOURCE_MISC_SHARED,
    D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION, D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC,
    D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIAdapter, IDXGIFactory1,
    IDXGIResource1,
};
use windows::core::{Interface, PCWSTR};

/// A picture: a BGRA shared texture, reference counted.
pub type Buffer = Arc<Shared>;

/// A BGRA Direct3D 11 texture with an NT shared handle, on its adapter's device.
pub struct Shared {
    texture: ID3D11Texture2D,
    handle: HANDLE,
    size: (u32, u32),
    gpu: Arc<Gpu>,
}

// Direct3D 11 textures and devices are free-threaded; the handle is a kernel handle.
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl Shared {
    /// The texture, on [`Shared::device`].
    pub fn texture(&self) -> &ID3D11Texture2D {
        &self.texture
    }

    /// Its NT shared handle, which stays the buffer's: open it (`OpenSharedHandle` on Direct3D
    /// 12, `OpenSharedResource1` on another Direct3D 11 device), don't close it.
    pub fn handle(&self) -> HANDLE {
        self.handle
    }

    /// Its size in pixels.
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// The device it was made on (multithread-protected).
    pub fn device(&self) -> &ID3D11Device {
        &self.gpu.device
    }

    /// The device's immediate context, used under [`Shared::lock`] only.
    pub fn context(&self) -> &ID3D11DeviceContext {
        &self.gpu.context
    }

    /// The immediate context's lock: hold it for every use of [`Shared::context`].
    pub fn lock(&self) -> parking_lot::MutexGuard<'_, ()> {
        self.gpu.lock.lock()
    }

    /// The graphics adapter it's on: a device on another adapter can't open it.
    pub fn adapter(&self) -> LUID {
        self.gpu.luid
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        // SAFETY: the handle was made for this texture and is closed once, here.
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}

/// A Direct3D 11 device on one adapter, shared by every buffer made there.
struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    luid: LUID,
    lock: Mutex<()>,
}

unsafe impl Send for Gpu {}
unsafe impl Sync for Gpu {}

/// The adapter buffers are made on (the compositor's), by LUID (low, high); `None`: the
/// system's first.
static ADAPTER: Mutex<Option<(u32, i32)>> = Mutex::new(None);
/// The device buffers are made on now.
static GPU: Mutex<Option<Arc<Gpu>>> = Mutex::new(None);

/// Makes later buffers on this graphics adapter (the compositor's: a shared texture opens only
/// on a device of the same adapter). Buffers already made keep theirs.
pub fn set_adapter(luid: Option<LUID>) {
    let new = luid.map(|l| (l.LowPart, l.HighPart));
    let mut adapter = ADAPTER.lock();
    if *adapter != new {
        *adapter = new;
        *GPU.lock() = None;
    }
}

fn gpu() -> Option<Arc<Gpu>> {
    let mut gpu = GPU.lock();
    if gpu.is_none() {
        let luid = ADAPTER.lock().map(|(low, high)| LUID { LowPart: low, HighPart: high });
        *gpu = make_gpu(luid).map(Arc::new);
    }
    gpu.clone()
}

fn make_gpu(luid: Option<LUID>) -> Option<Gpu> {
    let factory: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.map_err(|e| tracing::error!(%e, "buffers: opening DXGI")).ok()?;
    let mut i = 0;
    let adapter = loop {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(i) }) else {
            tracing::error!(?luid, "buffers: no such graphics adapter");
            return None;
        };
        i += 1;
        let Ok(desc) = (unsafe { adapter.GetDesc1() }) else { continue };
        if luid.is_none_or(|l| l.LowPart == desc.AdapterLuid.LowPart && l.HighPart == desc.AdapterLuid.HighPart) {
            break (adapter, desc.AdapterLuid);
        }
    };
    let (adapter, luid) = adapter;
    let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
    let (mut device, mut context) = (None, None);
    let made = unsafe {
        D3D11CreateDevice(
            &adapter.cast::<IDXGIAdapter>().ok()?,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    };
    if let Err(e) = made {
        tracing::error!(%e, "buffers: making a Direct3D 11 device");
        return None;
    }
    let (device, context): (ID3D11Device, ID3D11DeviceContext) = (device?, context?);
    if let Ok(threads) = device.cast::<ID3D11Multithread>() {
        unsafe {
            let _ = threads.SetMultithreadProtected(true);
        }
    }
    Some(Gpu { device, context, luid, lock: Mutex::new(()) })
}

/// The Direct3D 11 device buffers are made on now (multithread-protected), made if needed.
pub fn device() -> Option<ID3D11Device> {
    gpu().map(|g| g.device.clone())
}

/// A BGRA shared texture of `width` × `height`, drawn into and read on the GPU.
pub fn bgra_buffer(width: u32, height: u32) -> Option<Buffer> {
    make(width, height, None)
}

/// A new buffer holding `rgba` (`width` × `height`, rows packed), its colours as they are
/// (opaque pictures, or premultiplied ones).
pub fn from_rgba(rgba: &[u8], width: u32, height: u32) -> Option<Buffer> {
    let (w, h) = (width as usize, height as usize);
    if rgba.len() < w * h * 4 {
        return None;
    }
    let mut bgra = Vec::with_capacity(w * h * 4);
    for px in rgba[..w * h * 4].as_chunks::<4>().0 {
        bgra.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }
    make(width, height, Some(&bgra))
}

fn make(width: u32, height: u32, bgra: Option<&[u8]>) -> Option<Buffer> {
    if width == 0 || height == 0 {
        return None;
    }
    let gpu = gpu()?;
    let desc = D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_SHADER_RESOURCE.0 | D3D11_BIND_RENDER_TARGET.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
    };
    let initial = bgra.map(|b| D3D11_SUBRESOURCE_DATA {
        pSysMem: b.as_ptr().cast(),
        SysMemPitch: width * 4,
        SysMemSlicePitch: 0,
    });
    let mut texture = None;
    // SAFETY: the initial bytes (if any) are `height` rows of `width * 4`, alive for the call.
    if let Err(e) =
        unsafe { gpu.device.CreateTexture2D(&desc, initial.as_ref().map(|d| d as *const _), Some(&mut texture)) }
    {
        tracing::error!(%e, width, height, "buffers: making a shared texture");
        return None;
    }
    let texture: ID3D11Texture2D = texture?;
    let resource: IDXGIResource1 = texture.cast().ok()?;
    let handle = unsafe {
        resource.CreateSharedHandle(None, (DXGI_SHARED_RESOURCE_READ | DXGI_SHARED_RESOURCE_WRITE).0, PCWSTR::null())
    }
    .map_err(|e| tracing::error!(%e, "buffers: sharing a texture"))
    .ok()?;
    Some(Arc::new(Shared { texture, handle, size: (width, height), gpu }))
}
