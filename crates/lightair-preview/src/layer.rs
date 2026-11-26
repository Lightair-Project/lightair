//! Windows: the preview presented straight into a DirectComposition swap chain (the window's
//! canvas picture, under its UI), with no copy through memory and no redraw of the UI.
//!
//! The swap chain belongs to the device Lightair's own pictures are made on
//! (`lightair_host::buffers::device`), and each frame of Lightair's own compositor (a shared
//! texture) is copied into its back buffer (a GPU copy) and presented without waiting
//! ([`Layer::present_buffer`]). The app shows the swap chain in a composition visual under
//! GPUI's (`native_preview`).
//!
//! Frames are presented at the size the picture is shown at when that's smaller (the UI tells it:
//! [`Layer::show_at`]), scaled by the GPU's video processor: the swap chain's picture crosses to
//! the GPU that drives the display (a laptop's integrated one) every frame, and a preview a third
//! as wide is a ninth of that copying.
//!
//! Never waiting matters: the thread that presents draws the program for the outputs too, so a
//! present that blocked (the compositor busy, say while a display capture starts up and Windows
//! reconfigures it) would make the program skip frames. The swap chain has a frame-latency
//! waitable object; a frame is presented only when it's signalled (the compositor has taken the
//! last one), else the preview skips that frame and the program goes on.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};

use parking_lot::Mutex;
use windows::Win32::Foundation::{CloseHandle, HANDLE, RECT, WAIT_OBJECT_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_IGNORE, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM,
    DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_ERROR_WAS_STILL_DRAWING, DXGI_PRESENT_DO_NOT_WAIT,
    DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
    DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
    DXGI_USAGE_RENDER_TARGET_OUTPUT, IDXGIFactory2, IDXGISwapChain1, IDXGISwapChain2,
};
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::core::Interface;

/// A swap chain the preview presents its frames to, shown by a composition visual.
pub struct Layer {
    swap: IDXGISwapChain1,
    /// Signalled when the swap chain can take a frame without blocking (`Present` would wait
    /// otherwise); null where the system doesn't give one.
    ready: HANDLE,
    /// The back buffers' size, packed (width << 32 | height).
    size: AtomicU64,
    /// The back buffers' format (the presenting thread).
    format: Mutex<DXGI_FORMAT>,
    /// The size the picture is shown at (pixels, packed; 0: not known).
    wanted: AtomicU64,
    /// Serialises presenting with dropping.
    lock: Mutex<()>,
    /// The device the swap chain is on, and the pictures made on another device (of the same
    /// adapter) opened on it, each kept while its picture lives.
    device: ID3D11Device,
    opened: Mutex<Vec<(std::sync::Weak<lightair_host::buffers::Shared>, ID3D11Texture2D)>>,
    /// Scales frames down to the size shown (the presenting thread, under `lock`).
    scaler: Mutex<Option<Scaler>>,
}

/// The video processor scaling BGRA frames of one size to another.
struct Scaler {
    sizes: ((u32, u32), (u32, u32)),
    video: ID3D11VideoDevice,
    context: ID3D11VideoContext1,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
}

impl Scaler {
    fn new(device: &ID3D11Device, from: (u32, u32), to: (u32, u32)) -> Option<Scaler> {
        let video: ID3D11VideoDevice = device.cast().ok()?;
        let context: ID3D11VideoContext1 = unsafe { device.GetImmediateContext() }.ok()?.cast().ok()?;
        let rate = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: from.0,
            InputHeight: from.1,
            OutputFrameRate: rate,
            OutputWidth: to.0,
            OutputHeight: to.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_QUALITY,
        };
        unsafe {
            let enumerator = video.CreateVideoProcessorEnumerator(&content).ok()?;
            let flags = enumerator.CheckVideoProcessorFormat(DXGI_FORMAT_B8G8R8A8_UNORM).ok()?;
            let both =
                (D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_INPUT.0 | D3D11_VIDEO_PROCESSOR_FORMAT_SUPPORT_OUTPUT.0) as u32;
            if flags & both != both {
                return None;
            }
            let processor = video.CreateVideoProcessor(&enumerator, 0).ok()?;
            // RGB in and out: scaled, nothing converted
            context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            context.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            context.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            let all = |(w, h): (u32, u32)| RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
            context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&all(from)));
            context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&all(to)));
            context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&all(to)));
            Some(Scaler { sizes: (from, to), video, context, enumerator, processor })
        }
    }

    /// `src` scaled into `target`.
    unsafe fn scale(&self, src: &ID3D11Resource, target: &ID3D11Texture2D) -> bool {
        unsafe {
            let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                },
            };
            let mut input = None;
            if self.video.CreateVideoProcessorInputView(src, &self.enumerator, &in_desc, Some(&mut input)).is_err() {
                return false;
            }
            let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
            };
            let mut output = None;
            if self.video.CreateVideoProcessorOutputView(target, &self.enumerator, &out_desc, Some(&mut output)).is_err()
            {
                return false;
            }
            let Some(output) = output else { return false };
            let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(input),
                ..Default::default()
            }];
            let blitted = self.context.VideoProcessorBlt(&self.processor, &output, 0, &streams);
            // the stream's reference to the input view is ours to drop
            std::mem::ManuallyDrop::drop(&mut streams[0].pInputSurface);
            blitted.is_ok()
        }
    }
}

// SAFETY: the swap chain is used by one presenting thread at a time (under `lock`: resize, copy,
// present); other threads read `size` or hand the swap chain's pointer to DirectComposition,
// which takes its own reference. The waitable handle is a kernel handle; the devices are
// free-threaded and multithread-protected.
unsafe impl Send for Layer {}
unsafe impl Sync for Layer {}

const START: u32 = 16;
const FLAGS: DXGI_SWAP_CHAIN_FLAG = DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT;
/// Pictures from another device kept opened at most (a compositor's ring of output buffers).
const OPENED: usize = 8;

impl Layer {
    /// A swap chain on the device Lightair's own pictures are made on.
    pub fn new() -> Option<Self> {
        let layer = lightair_host::buffers::device().and_then(|d| Self::on(&d));
        if layer.is_none() {
            tracing::warn!("couldn't make a composition swap chain for the preview");
        }
        layer
    }

    fn on(device: &ID3D11Device) -> Option<Self> {
        let swap = unsafe { Self::make(device) }?;
        // at most one frame queued: the newest is what the window shows
        let ready = match swap.cast::<IDXGISwapChain2>() {
            Ok(s2) => unsafe {
                let _ = s2.SetMaximumFrameLatency(1);
                s2.GetFrameLatencyWaitableObject()
            },
            Err(_) => HANDLE::default(),
        };
        Some(Self {
            swap,
            ready,
            size: AtomicU64::new(pack(START, START)),
            format: Mutex::new(DXGI_FORMAT_B8G8R8A8_UNORM),
            wanted: AtomicU64::new(0),
            lock: Mutex::new(()),
            device: device.clone(),
            opened: Mutex::new(Vec::new()),
            scaler: Mutex::new(None),
        })
    }

    unsafe fn make(device: &ID3D11Device) -> Option<IDXGISwapChain1> {
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: START,
                Height: START,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                Flags: FLAGS.0 as u32,
            };
            factory.CreateSwapChainForComposition(device, &desc, None).ok()
        }
    }

    /// `IDXGISwapChain1*` (borrowed), for `IDCompositionVisual::SetContent`.
    pub fn swap_chain(&self) -> *mut c_void {
        self.swap.as_raw()
    }

    /// The size the picture is shown at, in pixels (the UI, when it places it).
    pub fn show_at(&self, w: u32, h: u32) {
        let packed = if w >= 2 && h >= 2 { pack(w & !1, h & !1) } else { 0 };
        self.wanted.store(packed, Ordering::Relaxed);
    }


    /// The back buffers' size in pixels (the picture's visual scales it to its place).
    pub fn size(&self) -> (u32, u32) {
        let p = self.size.load(Ordering::Relaxed);
        ((p >> 32) as u32, p as u32)
    }


    /// The size a `w`×`h` frame is presented at: the size shown when that's smaller (its aspect
    /// kept), else its own.
    fn presented_size(&self, (w, h): (u32, u32)) -> (u32, u32) {
        let wanted = self.wanted.load(Ordering::Relaxed);
        let ww = (wanted >> 32) as u32;
        if wanted == 0 || ww >= w || w == 0 {
            return (w, h);
        }
        let th = ((ww as u64 * h as u64 / w as u64) as u32 & !1).max(2);
        (ww.max(2), th)
    }

    /// Whether the swap chain can take a frame now, without `Present` waiting.
    fn can_take(&self) -> bool {
        self.ready.is_invalid() || unsafe { WaitForSingleObject(self.ready, 0) } == WAIT_OBJECT_0
    }

    /// Copies one of Lightair's own pictures (a frame of its compositor, finished on the GPU)
    /// into the back buffer and presents it, never waiting: when the compositor hasn't taken the
    /// last frame yet, this one is skipped. Whether it was presented.
    pub(crate) fn present_buffer(&self, buffer: &lightair_host::buffers::Buffer) -> bool {
        let Some(texture) = self.texture_of(buffer) else { return false };
        let Ok(src) = texture.cast::<ID3D11Resource>() else { return false };
        let (w, h) = buffer.size();
        unsafe { self.present_resource(&src, DXGI_FORMAT_B8G8R8A8_UNORM, w, h) }
    }

    /// `buffer`'s texture on the swap chain's device: its own, or opened from its handle (once
    /// per buffer).
    fn texture_of(&self, buffer: &lightair_host::buffers::Buffer) -> Option<ID3D11Texture2D> {
        if buffer.device().as_raw() == self.device.as_raw() {
            return Some(buffer.texture().clone());
        }
        let mut opened = self.opened.lock();
        opened.retain(|(b, _)| b.strong_count() > 0);
        if let Some((_, t)) = opened.iter().find(|(b, _)| std::ptr::eq(b.as_ptr(), std::sync::Arc::as_ptr(buffer))) {
            return Some(t.clone());
        }
        let device: windows::Win32::Graphics::Direct3D11::ID3D11Device1 = self.device.cast().ok()?;
        // SAFETY: the handle is the buffer's own NT handle to a shared texture, borrowed here.
        let texture: ID3D11Texture2D = match unsafe { device.OpenSharedResource1(buffer.handle()) } {
            Ok(t) => t,
            Err(e) => {
                tracing::debug!(%e, "the preview couldn't open a picture on its device");
                return None;
            }
        };
        if opened.len() >= OPENED {
            opened.remove(0);
        }
        opened.push((std::sync::Arc::downgrade(buffer), texture.clone()));
        Some(texture)
    }

    /// Copies `src` (`w`×`h`, in `format`) into the back buffer and presents it, never waiting.
    unsafe fn present_resource(&self, src: &ID3D11Resource, format: DXGI_FORMAT, w: u32, h: u32) -> bool {
        let _held = self.lock.lock();
        if !self.can_take() {
            return false;
        }
        // smaller: scaled down by the video processor (BGRA only), else copied as it is
        let to = if format == DXGI_FORMAT_B8G8R8A8_UNORM { self.presented_size((w, h)) } else { (w, h) };
        unsafe {
            let mut current = self.format.lock();
            let mut scaler = self.scaler.lock();
            let scaled = to != (w, h) && {
                if scaler.as_ref().is_none_or(|s| s.sizes != ((w, h), to)) {
                    *scaler = Scaler::new(&self.device, (w, h), to);
                }
                scaler.is_some()
            };
            let (bw, bh) = if scaled { to } else { (w, h) };
            if self.size() != (bw, bh) || *current != format {
                if self.swap.ResizeBuffers(0, bw, bh, format, FLAGS).is_err() {
                    return false;
                }
                self.size.store(pack(bw, bh), Ordering::Relaxed);
                *current = format;
            }
            let Ok(back) = self.swap.GetBuffer::<ID3D11Texture2D>(0) else { return false };
            let done = match scaler.as_ref().filter(|_| scaled) {
                Some(s) => s.scale(src, &back),
                None => {
                    let Ok(device) = back.GetDevice() else { return false };
                    let Ok(context) = device.GetImmediateContext() else { return false };
                    context.CopyResource(&back, src);
                    true
                }
            };
            drop(back);
            if !done {
                // this frame isn't shown; the next is copied at full size
                *scaler = None;
                self.wanted.store(0, Ordering::Relaxed);
                tracing::debug!("the preview couldn't scale its frame: shown at full size");
                return false;
            }
            let started = std::time::Instant::now();
            let hr = self.swap.Present(0, DXGI_PRESENT_DO_NOT_WAIT);
            let took = started.elapsed();
            if took > std::time::Duration::from_millis(4) {
                tracing::debug!(?took, "the preview's present waited");
            }
            if hr.is_err() && hr != DXGI_ERROR_WAS_STILL_DRAWING {
                tracing::debug!("preview present: {hr:?}");
            }
            hr.is_ok()
        }
    }
}

impl Drop for Layer {
    fn drop(&mut self) {
        if !self.ready.is_invalid() {
            unsafe {
                let _ = CloseHandle(self.ready);
            }
        }
    }
}


fn pack(w: u32, h: u32) -> u64 {
    ((w as u64) << 32) | h as u64
}
