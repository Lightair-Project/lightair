//! Windows: what Lightair's pictures travel in between the compositor, the previews and the
//! encoders, as IOSurface-backed pixel buffers do on macOS (`buffers.rs`): BGRA Direct3D 11
//! textures with NT shared handles, on one Direct3D 11 device per graphics adapter.
//!
//! The compositor (wgpu on Direct3D 12) opens a buffer by its handle and draws into it; the
//! window's preview copies it into its swap chain on the buffer's own device ([`Shared::device`]),
//! and Media Foundation's encoders take it the same way. Whoever draws into a buffer has finished
//! with it on the GPU before handing it on, so readers need no fence. Nothing here goes through
//! memory except [`from_rgba`] and [`Copier::from_memory`] (pictures made on the CPU).
//!
//! Pictures made on another device (a capture's shared textures) become buffers through a
//! [`Copier`]: it opens them by their handles, waits on the GPU for their shared fence and copies
//! them into buffers of its own.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1};
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_QUERY_DESC,
    D3D11_QUERY_EVENT, D3D11_RESOURCE_MISC_SHARED, D3D11_RESOURCE_MISC_SHARED_NTHANDLE, D3D11_SDK_VERSION,
    D3D11_SUBRESOURCE_DATA, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11CreateDevice, ID3D11Device, ID3D11Device1,
    ID3D11Device5, ID3D11DeviceContext, ID3D11DeviceContext4, ID3D11Fence, ID3D11Multithread, ID3D11Query,
    ID3D11Texture2D,
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

/// The graphics adapter buffers are made on now: what makes pictures for them (a capture) puts
/// its shared textures there, so they open on the buffers' device.
pub fn adapter() -> Option<LUID> {
    gpu().map(|g| g.luid)
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
    make_on(gpu()?, width, height, bgra)
}

fn make_on(gpu: Arc<Gpu>, width: u32, height: u32, bgra: Option<&[u8]>) -> Option<Buffer> {
    if width == 0 || height == 0 {
        return None;
    }
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

/// Buffers a [`Copier`] writes in turn, at most.
const COPIER_DEPTH: usize = 8;
/// A copier's buffers handed out lately aren't written again before the others: a reader may
/// still have work in flight on one it has let go.
const COPIER_RECENT: usize = 2;
/// How long a copy may take on the GPU before it's given up.
const COPY_TIMEOUT: Duration = Duration::from_millis(250);

/// Copies one stream of pictures made elsewhere (a capture's shared textures on the same
/// graphics adapter, or bytes) into buffers of its own, which it writes in turn: a buffer is
/// written again only once nothing else holds it. Each copy has finished on the GPU when it's
/// handed out, as every buffer has.
pub struct Copier {
    gpu: Option<Arc<Gpu>>,
    /// Shared textures opened on the buffers' device: their handle, size and texture. A capture
    /// writes each of its pictures into one of a few textures, each always with the same handle.
    opened: Vec<(usize, (u32, u32), ID3D11Texture2D)>,
    /// The shared fence those textures' writes signal: its handle, and it opened here.
    fence: Option<(usize, ID3D11Fence)>,
    pool: Vec<Buffer>,
    /// The pool's buffers handed out lately, newest last.
    recent: VecDeque<usize>,
    /// Says when the GPU has done the work before it.
    done: Option<ID3D11Query>,
    /// A texture didn't open (another adapter): said once.
    said: bool,
}

// The textures, fence and query are used under the device's lock only.
unsafe impl Send for Copier {}

impl Default for Copier {
    fn default() -> Self {
        Copier::new()
    }
}

impl Copier {
    pub fn new() -> Copier {
        Copier {
            gpu: None,
            opened: Vec::new(),
            fence: None,
            pool: Vec::new(),
            recent: VecDeque::new(),
            done: None,
            said: false,
        }
    }

    /// A copy of the BGRA shared texture `handle` (`size`, an NT handle that stays its
    /// owner's), once the shared fence `fence` (its NT handle and the value it reaches when the
    /// texture is written) says it's written. `None` when it can't be opened (another graphics
    /// adapter) or every buffer is still held. Blocks until the copy is done.
    pub fn copy(&mut self, handle: HANDLE, size: (u32, u32), fence: Option<(HANDLE, u64)>) -> Option<Buffer> {
        let gpu = self.gpu()?;
        let source = self.open(&gpu, handle, size)?;
        let fence = match fence {
            Some((handle, value)) => Some((self.open_fence(&gpu, handle)?, value)),
            None => None,
        };
        let target = self.take(&gpu, size)?;
        {
            let _lock = gpu.lock.lock();
            if let Some((fence, value)) = &fence {
                let context: ID3D11DeviceContext4 = gpu.context.cast().ok()?;
                // SAFETY: the fence was opened on this device; the context is used under its lock.
                unsafe { context.Wait(fence, *value) }.ok()?;
            }
            // SAFETY: both textures are BGRA of `size` on this device.
            unsafe { gpu.context.CopyResource(target.texture(), &source) };
        }
        self.finish(&gpu).then_some(target)
    }

    /// [`Copier::copy`] of a handle that's only valid for this call (Chromium's paint): opened
    /// for the copy and let go after, since the same value may name another texture later.
    pub fn copy_once(&mut self, handle: HANDLE, size: (u32, u32)) -> Option<Buffer> {
        let copied = self.copy(handle, size, None);
        let key = handle.0 as usize;
        self.opened.retain(|(h, _, _)| *h != key);
        copied
    }

    /// A buffer holding a picture in memory: `size`, BGRA rows `stride` bytes apart. `None`
    /// when `data` is too short or every buffer is still held. Blocks until it's written.
    pub fn from_memory(&mut self, data: &[u8], stride: usize, size: (u32, u32)) -> Option<Buffer> {
        if stride < size.0 as usize * 4 || data.len() < stride * size.1.saturating_sub(1) as usize + size.0 as usize * 4
        {
            return None;
        }
        let gpu = self.gpu()?;
        let target = self.take(&gpu, size)?;
        {
            let _lock = gpu.lock.lock();
            // SAFETY: `data` holds `size.1` rows of `size.0` BGRA pixels, `stride` bytes apart.
            unsafe { gpu.context.UpdateSubresource(target.texture(), 0, None, data.as_ptr().cast(), stride as u32, 0) };
        }
        self.finish(&gpu).then_some(target)
    }

    /// The buffers' device; what was opened on an older one is let go.
    fn gpu(&mut self) -> Option<Arc<Gpu>> {
        let now = gpu()?;
        if !self.gpu.as_ref().is_some_and(|g| Arc::ptr_eq(g, &now)) {
            self.opened.clear();
            self.fence = None;
            self.pool.clear();
            self.recent.clear();
            self.done = None;
            self.gpu = Some(now.clone());
        }
        Some(now)
    }

    /// The shared texture `handle` of `size` on the buffers' device.
    fn open(&mut self, gpu: &Gpu, handle: HANDLE, size: (u32, u32)) -> Option<ID3D11Texture2D> {
        let key = handle.0 as usize;
        if let Some((_, _, texture)) = self.opened.iter().find(|(h, s, _)| *h == key && *s == size) {
            return Some(texture.clone());
        }
        // a texture of another size: the capture made new ones
        self.opened.retain(|(_, s, _)| *s == size);
        let device: ID3D11Device1 = gpu.device.cast().ok()?;
        // SAFETY: `handle` is an NT handle to a shared texture, borrowed for the call.
        let texture: ID3D11Texture2D = match unsafe { device.OpenSharedResource1(handle) } {
            Ok(t) => t,
            Err(e) => {
                if !std::mem::replace(&mut self.said, true) {
                    tracing::warn!(%e, "buffers: a picture from another device didn't open (another graphics adapter?)");
                }
                return None;
            }
        };
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        // SAFETY: only reads the texture's description.
        unsafe { texture.GetDesc(&mut desc) };
        if (desc.Width, desc.Height) != size {
            return None;
        }
        if self.opened.len() >= 16 {
            self.opened.remove(0);
        }
        self.opened.push((key, size, texture.clone()));
        Some(texture)
    }

    /// The shared fence `handle` on the buffers' device.
    fn open_fence(&mut self, gpu: &Gpu, handle: HANDLE) -> Option<ID3D11Fence> {
        let key = handle.0 as usize;
        if let Some((h, fence)) = &self.fence
            && *h == key
        {
            return Some(fence.clone());
        }
        let device: ID3D11Device5 = gpu.device.cast().ok()?;
        let mut fence: Option<ID3D11Fence> = None;
        // SAFETY: `handle` is an NT handle to a shared fence, borrowed for the call.
        if let Err(e) = unsafe { device.OpenSharedFence(handle, &mut fence) } {
            tracing::warn!(%e, "buffers: a shared fence didn't open");
            return None;
        }
        let fence = fence?;
        self.fence = Some((key, fence.clone()));
        Some(fence)
    }

    /// A buffer of `size` to write: one nothing else holds that wasn't handed out lately, else
    /// a new one while there are fewer than [`COPIER_DEPTH`], else any nothing holds.
    fn take(&mut self, gpu: &Arc<Gpu>, size: (u32, u32)) -> Option<Buffer> {
        if self.pool.first().is_some_and(|b| b.size != size) {
            self.pool.clear();
            self.recent.clear();
        }
        let free = |b: &Buffer| Arc::strong_count(b) == 1;
        let i = (0..self.pool.len())
            .find(|i| !self.recent.contains(i) && free(&self.pool[*i]))
            .or_else(|| {
                (self.pool.len() < COPIER_DEPTH)
                    .then(|| {
                        let buffer = make_on(gpu.clone(), size.0, size.1, None)?;
                        self.pool.push(buffer);
                        Some(self.pool.len() - 1)
                    })
                    .flatten()
            })
            .or_else(|| (0..self.pool.len()).find(|i| free(&self.pool[*i])))?;
        self.recent.retain(|r| *r != i);
        self.recent.push_back(i);
        while self.recent.len() > COPIER_RECENT {
            self.recent.pop_front();
        }
        Some(self.pool[i].clone())
    }

    /// Waits until the GPU has done the work given so far; false when it takes too long.
    fn finish(&mut self, gpu: &Gpu) -> bool {
        if self.done.is_none() {
            let desc = D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 };
            let mut query = None;
            // SAFETY: makes an event query on this device.
            if unsafe { gpu.device.CreateQuery(&desc, Some(&mut query)) }.is_err() {
                return false;
            }
            self.done = query;
        }
        let Some(query) = &self.done else { return false };
        {
            let _lock = gpu.lock.lock();
            // SAFETY: the context is used under its lock.
            unsafe {
                gpu.context.End(query);
                gpu.context.Flush();
            }
        }
        let start = Instant::now();
        loop {
            let mut finished: i32 = 0;
            let asked = {
                let _lock = gpu.lock.lock();
                // SAFETY: an event query's data is a BOOL, written once the work is done.
                unsafe {
                    gpu.context.GetData(query, Some((&mut finished as *mut i32).cast()), size_of::<i32>() as u32, 0)
                }
            };
            if asked.is_err() {
                return false;
            }
            if finished != 0 {
                return true;
            }
            if start.elapsed() > COPY_TIMEOUT {
                tracing::warn!("buffers: a copy took too long on the GPU");
                return false;
            }
            std::thread::sleep(Duration::from_micros(200));
        }
    }
}
