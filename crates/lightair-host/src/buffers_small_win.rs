//! Windows: Lightair's pictures read back small, for what analyses them on the CPU (the looks'
//! face tracker, face detection for Face blur and Auto-frame): each picture scaled down on the
//! GPU by its device's video processor, copied to a staging texture and read into memory. The
//! macOS side scales with VideoToolbox and reads the pixel buffer's memory directly.

use std::mem::ManuallyDrop;

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::core::Interface;

use crate::buffers::Buffer;

/// Scales one stream of pictures down and reads them back, keeping its textures between calls.
#[derive(Default)]
pub struct Shrinker {
    made: Option<Made>,
}

// The textures and video objects are used under their device's lock only.
unsafe impl Send for Shrinker {}

/// What's made for one device and pair of sizes.
struct Made {
    device: usize,
    from: (u32, u32),
    to: (u32, u32),
    /// The video processor scaling into `small` (none when the sizes are the same).
    scale: Option<(ID3D11VideoDevice, ID3D11VideoContext1, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor)>,
    small: Option<ID3D11Texture2D>,
    staging: ID3D11Texture2D,
}

impl Shrinker {
    pub fn new() -> Shrinker {
        Shrinker::default()
    }

    /// `buffer` scaled to `to` (at most its own size) as tightly packed BGRA rows; `None` when
    /// the GPU can't do it. Waits for the GPU.
    pub fn read(&mut self, buffer: &Buffer, to: (u32, u32)) -> Option<Vec<u8>> {
        let from = buffer.size();
        let to = (to.0.clamp(1, from.0.max(1)), to.1.clamp(1, from.1.max(1)));
        let device = buffer.device();
        let key = device.as_raw() as usize;
        if self.made.as_ref().is_none_or(|m| (m.device, m.from, m.to) != (key, from, to)) {
            self.made = Made::new(device, from, to);
        }
        let m = self.made.as_ref()?;
        let _lock = buffer.lock();
        let context = buffer.context();
        let source: ID3D11Resource = match (&m.scale, &m.small) {
            (Some(scale), Some(small)) => {
                // SAFETY: the views are made for these textures on this device; the context is
                // used under its lock.
                unsafe { blt(scale, buffer.texture(), small) }?;
                small.cast().ok()?
            }
            _ => buffer.texture().cast().ok()?,
        };
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: the staging texture is `to` big, BGRA, readable; mapping waits for the copy.
        unsafe {
            context.CopyResource(&m.staging, &source);
            context.Map(&m.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).ok()?;
        }
        let (w, h) = (to.0 as usize, to.1 as usize);
        let mut out = Vec::with_capacity(w * h * 4);
        // SAFETY: the mapped texture holds `h` rows of `RowPitch` bytes, each at least `w` × 4
        unsafe {
            let base = mapped.pData as *const u8;
            for y in 0..h {
                out.extend_from_slice(std::slice::from_raw_parts(base.add(y * mapped.RowPitch as usize), w * 4));
            }
            context.Unmap(&m.staging, 0);
        }
        Some(out)
    }
}

impl Made {
    fn new(device: &ID3D11Device, from: (u32, u32), to: (u32, u32)) -> Option<Made> {
        let texture = |usage: D3D11_USAGE, bind: u32, cpu: u32| {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: to.0,
                Height: to.1,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: usage,
                BindFlags: bind,
                CPUAccessFlags: cpu,
                MiscFlags: 0,
            };
            let mut t = None;
            // SAFETY: makes a texture on this device.
            unsafe { device.CreateTexture2D(&desc, None, Some(&mut t)) }.ok()?;
            t
        };
        let staging = texture(D3D11_USAGE_STAGING, 0, D3D11_CPU_ACCESS_READ.0 as u32)?;
        let (scale, small) = if from == to {
            (None, None)
        } else {
            let small = texture(D3D11_USAGE_DEFAULT, D3D11_BIND_RENDER_TARGET.0 as u32, 0)?;
            (Some(scaler(device, from, to)?), Some(small))
        };
        Some(Made { device: device.as_raw() as usize, from, to, scale, small, staging })
    }
}

/// The video processor scaling BGRA of `from` to BGRA of `to` (RGB in and out: nothing converted).
fn scaler(
    device: &ID3D11Device,
    from: (u32, u32),
    to: (u32, u32),
) -> Option<(ID3D11VideoDevice, ID3D11VideoContext1, ID3D11VideoProcessorEnumerator, ID3D11VideoProcessor)> {
    let video: ID3D11VideoDevice = device.cast().ok()?;
    // SAFETY: the immediate context is only cast here; it's used under the device's lock.
    let context: ID3D11VideoContext1 = unsafe { device.GetImmediateContext() }.ok()?.cast().ok()?;
    let rate = DXGI_RATIONAL { Numerator: 30, Denominator: 1 };
    let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
        InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
        InputFrameRate: rate,
        InputWidth: from.0,
        InputHeight: from.1,
        OutputFrameRate: rate,
        OutputWidth: to.0,
        OutputHeight: to.1,
        Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
    };
    // SAFETY: makes and sets up a video processor on this device.
    unsafe {
        let enumerator = video.CreateVideoProcessorEnumerator(&content).ok()?;
        let processor = video.CreateVideoProcessor(&enumerator, 0).ok()?;
        context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
        context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
        context.VideoProcessorSetStreamColorSpace1(&processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
        context.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
        let all = |(w, h): (u32, u32)| RECT { left: 0, top: 0, right: w as i32, bottom: h as i32 };
        context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&all(from)));
        context.VideoProcessorSetStreamDestRect(&processor, 0, true, Some(&all(to)));
        context.VideoProcessorSetOutputTargetRect(&processor, true, Some(&all(to)));
        Some((video, context, enumerator, processor))
    }
}

/// `src` scaled into `target` by the video processor.
///
/// # Safety
/// Both textures are BGRA on the processor's device; the immediate context is held.
unsafe fn blt(
    (video, context, enumerator, processor): &(
        ID3D11VideoDevice,
        ID3D11VideoContext1,
        ID3D11VideoProcessorEnumerator,
        ID3D11VideoProcessor,
    ),
    src: &ID3D11Texture2D,
    target: &ID3D11Texture2D,
) -> Option<()> {
    unsafe {
        let in_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 } },
        };
        let mut input = None;
        video.CreateVideoProcessorInputView(src, enumerator, &in_desc, Some(&mut input)).ok()?;
        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut output = None;
        video.CreateVideoProcessorOutputView(target, enumerator, &out_desc, Some(&mut output)).ok()?;
        let mut streams =
            [D3D11_VIDEO_PROCESSOR_STREAM { Enable: true.into(), pInputSurface: ManuallyDrop::new(input), ..Default::default() }];
        let done = context.VideoProcessorBlt(processor, output.as_ref()?, 0, &streams);
        // the stream's reference to the input view is ours to drop
        ManuallyDrop::drop(&mut streams[0].pInputSurface);
        done.ok()
    }
}
