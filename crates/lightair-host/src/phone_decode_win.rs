//! A phone camera's stream (and another computer's over SRT) decoded the moment it arrives
//! (Windows), as `phone_decode` does on macOS: each video unit through Media Foundation's H.264
//! or HEVC decoder on the GPU (low latency: a picture comes out for each unit that goes in), each
//! AAC frame through its AAC decoder. No demuxer probing, no pacing by timestamps, no queues.
//!
//! Units come as the app split them out of the MPEG-TS (Annex B, parameter sets before every
//! keyframe); a change of parameter sets makes a new decoder. Each decoded picture (NV12, in the
//! decoder's own texture) is converted to BGRA by the GPU's video processor into a texture of
//! the decoder's, then copied into a Lightair buffer ([`crate::buffers::Buffer`]) for the
//! compositor. Without a Direct3D decoder the NV12 picture is converted in memory.

use std::mem::ManuallyDrop;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE, RECT};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::{DXGI_SHARED_RESOURCE_READ, DXGI_SHARED_RESOURCE_WRITE, IDXGIResource1};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoTaskMemFree};
use windows::core::{BOOL, GUID, Interface};

use crate::buffers::{Buffer, Copier};
use crate::phone_units::{adts, nals, parameter_set};

/// Where a decoder's pictures go: each decoded picture (a BGRA Lightair buffer, borrowed for the
/// call) and its PTS on the 90 kHz clock, when it has one. Called on the thread that decodes.
pub type Pictures = Arc<dyn Fn(&Buffer, Option<u64>) + Send + Sync>;

/// Media Foundation started for as long as a decoder lives.
pub(crate) struct Mf;

impl Mf {
    pub(crate) fn start() -> Option<Mf> {
        // S_FALSE (already) and RPC_E_CHANGED_MODE (a single-threaded apartment) both leave the
        // thread able to use Media Foundation
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.ok().map(|()| Mf)
    }
}

impl Drop for Mf {
    fn drop(&mut self) {
        let _ = unsafe { MFShutdown() };
    }
}

/// The first transform of `category` taking `subtype` of `major` in, made (synchronous ones only:
/// they decode as each unit goes in).
fn transform(category: GUID, major: GUID, subtype: GUID) -> Option<IMFTransform> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: major, guidSubtype: subtype };
    let mut array: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    unsafe {
        MFTEnumEx(
            category,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            None,
            &mut array,
            &mut count,
        )
    }
    .ok()?;
    if array.is_null() {
        return None;
    }
    let mut found = None;
    for i in 0..count as usize {
        // each element's reference moves out of the array, which is then freed
        let Some(activate) = (unsafe { std::ptr::read(array.add(i)) }) else { continue };
        if found.is_none() {
            found = unsafe { activate.ActivateObject::<IMFTransform>() }.ok();
        }
    }
    unsafe { CoTaskMemFree(Some(array as *const _)) };
    found
}

/// A sample holding `bytes` in a new memory buffer, at `time_hns`.
fn memory_sample(bytes: &[u8], time_hns: i64) -> Option<IMFSample> {
    unsafe {
        let buffer = MFCreateMemoryBuffer(bytes.len().max(1) as u32).ok()?;
        let mut at: *mut u8 = std::ptr::null_mut();
        buffer.Lock(&mut at, None, None).ok()?;
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), at, bytes.len());
        buffer.Unlock().ok()?;
        buffer.SetCurrentLength(bytes.len() as u32).ok()?;
        let sample = MFCreateSample().ok()?;
        sample.AddBuffer(&buffer).ok()?;
        let _ = sample.SetSampleTime(time_hns);
        Some(sample)
    }
}

/// Takes every output the transform has ready, handing each sample to `each`; on a format change
/// `renegotiate` is called and taking goes on.
fn drain(mft: &IMFTransform, mut renegotiate: impl FnMut(&IMFTransform) -> bool, mut each: impl FnMut(IMFSample)) {
    for _ in 0..16 {
        let Ok(info) = (unsafe { mft.GetOutputStreamInfo(0) }) else { return };
        let provides = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32
            != 0;
        let given = if provides {
            None
        } else {
            let Some(s) = memory_sample(&vec![0u8; info.cbSize.max(4096) as usize], 0) else { return };
            // the buffer is empty until the transform writes it
            if let Ok(b) = unsafe { s.GetBufferByIndex(0) } {
                let _ = unsafe { b.SetCurrentLength(0) };
            }
            Some(s)
        };
        let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(given),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0u32;
        let result = unsafe { mft.ProcessOutput(0, &mut buffers, &mut status) };
        let [buffer] = buffers;
        let sample = ManuallyDrop::into_inner(buffer.pSample);
        drop(ManuallyDrop::into_inner(buffer.pEvents));
        match result {
            Ok(()) => {
                if let Some(s) = sample {
                    each(s);
                }
            }
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                if !renegotiate(mft) {
                    return;
                }
            }
            Err(_) => return,
        }
    }
}

// ---------------------------------------------------------------------------------- video

/// The GPU side: a video device on the adapter Lightair's buffers are on, the device manager the
/// decoder uses it through, and the conversion to BGRA.
pub(crate) struct Gpu {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    pub(crate) manager: IMFDXGIDeviceManager,
    video: ID3D11VideoDevice,
    video_context: ID3D11VideoContext1,
    query: ID3D11Query,
    convert: Option<Convert>,
}

// SAFETY: a multithread-protected device; used by the decoding thread only.
unsafe impl Send for Gpu {}

impl Gpu {
    pub(crate) fn new() -> Option<Gpu> {
        let device = shared_video_device()?;
        let context = unsafe { device.GetImmediateContext() }.ok()?;
        let mut token = 0u32;
        let mut manager = None;
        unsafe { MFCreateDXGIDeviceManager(&mut token, &mut manager) }.ok()?;
        let manager = manager?;
        unsafe { manager.ResetDevice(&device, token) }.ok()?;
        let video: ID3D11VideoDevice = device.cast().ok()?;
        let video_context: ID3D11VideoContext1 = context.cast().ok()?;
        let desc = D3D11_QUERY_DESC { Query: D3D11_QUERY_EVENT, MiscFlags: 0 };
        let mut query = None;
        unsafe { device.CreateQuery(&desc, Some(&mut query)) }.ok()?;
        Some(Gpu { device, context, manager, video, video_context, query: query?, convert: None })
    }

    /// The conversion for `texture`'s pictures at `shown` size, turned by `rotation`.
    fn convert_for(&mut self, texture: &ID3D11Texture2D, shown: (u32, u32), rotation: u32) -> Option<&mut Convert> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };
        let coded = (desc.Width, desc.Height);
        let key = (coded, shown, rotation);
        if self.convert.as_ref().is_none_or(|c| c.key != key) {
            self.convert = Convert::new(&self.video, &self.video_context, coded, shown, rotation);
        }
        self.convert.as_mut()
    }

    /// Whether Lightair's buffers are on this decoder's device (its pictures converted straight
    /// into them).
    fn shares_buffers(&self) -> bool {
        crate::buffers::device().is_some_and(|d| d.as_raw() == self.device.as_raw())
    }

    /// The video processor's pass from `texture`'s slice into `output`.
    fn blt(
        &self,
        texture: &ID3D11Texture2D,
        slice: u32,
        enumerator: &ID3D11VideoProcessorEnumerator,
        processor: &ID3D11VideoProcessor,
        output: &ID3D11VideoProcessorOutputView,
    ) -> bool {
        let view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: slice },
            },
        };
        let mut input = None;
        if unsafe { self.video.CreateVideoProcessorInputView(texture, enumerator, &view_desc, Some(&mut input)) }.is_err() {
            return false;
        }
        let mut streams =
            [D3D11_VIDEO_PROCESSOR_STREAM { Enable: true.into(), pInputSurface: ManuallyDrop::new(input), ..Default::default() }];
        let blitted = unsafe { self.video_context.VideoProcessorBlt(processor, output, 0, &streams) };
        // the stream's reference to the input view is ours to drop
        unsafe { ManuallyDrop::drop(&mut streams[0].pInputSurface) };
        blitted.is_ok()
    }

    /// `texture`'s picture (slice `slice`, NV12) converted into `target` (one of Lightair's
    /// buffers, on this device, the conversion's size). The GPU finishes it later: the caller
    /// waits (`Copier::draw`).
    fn bgra_into(&mut self, texture: &ID3D11Texture2D, slice: u32, shown: (u32, u32), rotation: u32, target: &ID3D11Texture2D) -> bool {
        let video = self.video.clone();
        let Some(c) = self.convert_for(texture, shown, rotation) else { return false };
        let Some(output) = c.view_of(&video, target) else { return false };
        let (enumerator, processor) = (c.enumerator.clone(), c.processor.clone());
        self.blt(texture, slice, &enumerator, &processor, &output)
    }

    /// The size `texture`'s pictures convert to at `shown`, turned by `rotation`.
    fn out_size(&mut self, texture: &ID3D11Texture2D, shown: (u32, u32), rotation: u32) -> Option<(u32, u32)> {
        self.convert_for(texture, shown, rotation).map(|c| c.out)
    }

    /// `texture`'s picture (slice `slice`, NV12) as BGRA of `shown` size in the conversion's own
    /// shared texture, finished on the GPU: its NT handle and size (for a decoder whose device
    /// isn't the buffers').
    fn bgra_of(&mut self, texture: &ID3D11Texture2D, slice: u32, shown: (u32, u32), rotation: u32) -> Option<(HANDLE, (u32, u32))> {
        let (device, video) = (self.device.clone(), self.video.clone());
        let c = self.convert_for(texture, shown, rotation)?;
        let out = c.out;
        let (enumerator, processor) = (c.enumerator.clone(), c.processor.clone());
        let own = c.own(&device, &video)?;
        let (output, handle) = (own.output.clone(), own.handle);
        if !self.blt(texture, slice, &enumerator, &processor, &output) {
            return None;
        }
        // finished before another device copies it
        unsafe {
            self.context.End(&self.query);
            self.context.Flush();
        }
        let deadline = Instant::now() + Duration::from_millis(100);
        loop {
            let mut done = BOOL(0);
            let read = unsafe {
                self.context.GetData(&self.query, Some(&mut done as *mut BOOL as *mut _), size_of::<BOOL>() as u32, 0)
            };
            if read.is_ok() && done.as_bool() {
                break;
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::yield_now();
        }
        Some((handle, out))
    }
}

/// The video device every decoder shares (media files, phone cameras, SRT links): the buffers'
/// own device when it has video support, else one made on the buffers' adapter. A device each
/// costs its own driver memory (about 20 MB). It is multithread-protected; each decoder keeps
/// its own device manager, query and conversion. Made again if the buffers' adapter changes.
fn shared_video_device() -> Option<ID3D11Device> {
    if let Some(device) = crate::buffers::device()
        && device.cast::<ID3D11VideoDevice>().is_ok()
    {
        return Some(device);
    }
    struct Shared(windows::Win32::Foundation::LUID, ID3D11Device);
    // SAFETY: Direct3D 11 devices are free-threaded; this one is multithread-protected.
    unsafe impl Send for Shared {}
    static SHARED: std::sync::Mutex<Option<Shared>> = std::sync::Mutex::new(None);
    let buffers = crate::buffers::device();
    let luid = crate::buffers::adapter().unwrap_or_default();
    let mut shared = SHARED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(Shared(at, device)) = shared.as_ref()
        && *at == luid
    {
        return Some(device.clone());
    }
    let device = lightair_encode::windows::video_device(buffers.as_ref()).ok()?;
    *shared = Some(Shared(luid, device.clone()));
    Some(device)
}

/// NV12 of one size to BGRA of another (turned clockwise by `rotation` degrees): into Lightair's
/// buffers directly when they're on the decoder's device, else into a shared texture of its own
/// ([`Own`], made the first time it's needed).
struct Convert {
    key: ((u32, u32), (u32, u32), u32),
    /// The turned size.
    out: (u32, u32),
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    own: Option<Own>,
}

/// The conversion's own shared texture, for a decoder on a device of its own.
struct Own {
    output: ID3D11VideoProcessorOutputView,
    _target: ID3D11Texture2D,
    handle: HANDLE,
}

impl Convert {
    fn new(
        video: &ID3D11VideoDevice,
        context: &ID3D11VideoContext1,
        coded: (u32, u32),
        shown: (u32, u32),
        rotation: u32,
    ) -> Option<Convert> {
        let turned = rotation == 90 || rotation == 270;
        let out = if turned { (shown.1, shown.0) } else { shown };
        let rate = DXGI_RATIONAL { Numerator: 30, Denominator: 1 };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rate,
            InputWidth: coded.0,
            InputHeight: coded.1,
            OutputFrameRate: rate,
            OutputWidth: out.0,
            OutputHeight: out.1,
            Usage: D3D11_VIDEO_USAGE_OPTIMAL_SPEED,
        };
        let enumerator = unsafe { video.CreateVideoProcessorEnumerator(&content) }.ok()?;
        let processor = unsafe { video.CreateVideoProcessor(&enumerator, 0) }.ok()?;
        // HD's colours from 720 lines up, SD's below; the picture's coded margin cut off
        let input_space = if shown.1 >= 720 {
            DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P709
        } else {
            DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601
        };
        let source = RECT { left: 0, top: 0, right: shown.0.min(coded.0) as i32, bottom: shown.1.min(coded.1) as i32 };
        unsafe {
            context.VideoProcessorSetStreamFrameFormat(&processor, 0, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
            context.VideoProcessorSetStreamAutoProcessingMode(&processor, 0, false);
            context.VideoProcessorSetStreamColorSpace1(&processor, 0, input_space);
            context.VideoProcessorSetOutputColorSpace1(&processor, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&source));
            let turn = match rotation {
                90 => D3D11_VIDEO_PROCESSOR_ROTATION_90,
                180 => D3D11_VIDEO_PROCESSOR_ROTATION_180,
                270 => D3D11_VIDEO_PROCESSOR_ROTATION_270,
                _ => D3D11_VIDEO_PROCESSOR_ROTATION_IDENTITY,
            };
            context.VideoProcessorSetStreamRotation(&processor, 0, rotation != 0, turn);
        }
        Some(Convert { key: (coded, shown, rotation), out, enumerator, processor, own: None })
    }

    /// Its own shared texture to convert into, made once.
    fn own(&mut self, device: &ID3D11Device, video: &ID3D11VideoDevice) -> Option<&Own> {
        if self.own.is_none() {
            self.own = Own::new(device, video, &self.enumerator, self.out);
        }
        self.own.as_ref()
    }

    /// An output view of `target` (BGRA, [`Convert::out`] big) for this conversion.
    fn view_of(&self, video: &ID3D11VideoDevice, target: &ID3D11Texture2D) -> Option<ID3D11VideoProcessorOutputView> {
        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut output = None;
        unsafe { video.CreateVideoProcessorOutputView(target, &self.enumerator, &out_desc, Some(&mut output)) }.ok()?;
        output
    }
}

impl Own {
    fn new(
        device: &ID3D11Device,
        video: &ID3D11VideoDevice,
        enumerator: &ID3D11VideoProcessorEnumerator,
        out: (u32, u32),
    ) -> Option<Own> {
        let desc = D3D11_TEXTURE2D_DESC {
            Width: out.0,
            Height: out.1,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
            CPUAccessFlags: 0,
            MiscFlags: (D3D11_RESOURCE_MISC_SHARED.0 | D3D11_RESOURCE_MISC_SHARED_NTHANDLE.0) as u32,
        };
        let mut target = None;
        unsafe { device.CreateTexture2D(&desc, None, Some(&mut target)) }.ok()?;
        let target = target?;
        let handle = unsafe {
            target.cast::<IDXGIResource1>().ok()?.CreateSharedHandle(
                None,
                DXGI_SHARED_RESOURCE_READ.0 | DXGI_SHARED_RESOURCE_WRITE.0,
                None,
            )
        }
        .ok()?;
        let out_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 { Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 } },
        };
        let mut output = None;
        unsafe { video.CreateVideoProcessorOutputView(&target, enumerator, &out_desc, Some(&mut output)) }.ok()?;
        Some(Own { output: output?, _target: target, handle })
    }
}

impl Drop for Own {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

/// A Media Foundation decoder for one set of parameter sets.
struct Video {
    hevc: bool,
    params: Vec<Vec<u8>>,
    mft: IMFTransform,
    /// The decoder draws into Direct3D textures (else memory).
    gpu: bool,
    /// The pictures' coded size and the size shown (the coded margin cut off).
    coded: (u32, u32),
    shown: (u32, u32),
}

impl Video {
    fn new(hevc: bool, params: Vec<Vec<u8>>, gpu: Option<&Gpu>) -> Option<Video> {
        let subtype = if hevc { MFVideoFormat_HEVC } else { MFVideoFormat_H264 };
        let Some(mft) = transform(MFT_CATEGORY_VIDEO_DECODER, MFMediaType_Video, subtype) else {
            tracing::warn!(hevc, "phone camera: Windows has no decoder for it");
            return None;
        };
        let attributes = unsafe { mft.GetAttributes() }.ok();
        if let Some(a) = &attributes {
            // a picture out for each unit in, not a few frames later
            let _ = unsafe { a.SetUINT32(&MF_LOW_LATENCY, 1) };
        }
        let d3d = attributes.as_ref().is_some_and(|a| unsafe { a.GetUINT32(&MF_SA_D3D11_AWARE) }.unwrap_or(0) != 0);
        let gpu = match gpu {
            Some(g) if d3d => unsafe { mft.ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, g.manager.as_raw() as usize) }
                .is_ok(),
            _ => false,
        };
        unsafe {
            let input = MFCreateMediaType().ok()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok()?;
            input.SetGUID(&MF_MT_SUBTYPE, &subtype).ok()?;
            let _ = input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
            mft.SetInputType(0, &input, 0).ok()?;
        }
        let mut v = Video { hevc, params, mft, gpu, coded: (0, 0), shown: (0, 0) };
        v.choose_output();
        unsafe {
            let _ = v.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = v.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
        }
        Some(v)
    }

    /// NV12 out, and the sizes it gives; `false` when it offers none.
    fn choose_output(&mut self) -> bool {
        for i in 0..32 {
            let Ok(t) = (unsafe { self.mft.GetOutputAvailableType(0, i) }) else { return false };
            if unsafe { t.GetGUID(&MF_MT_SUBTYPE) }.ok() != Some(MFVideoFormat_NV12) {
                continue;
            }
            if unsafe { self.mft.SetOutputType(0, &t, 0) }.is_err() {
                return false;
            }
            let packed = unsafe { t.GetUINT64(&MF_MT_FRAME_SIZE) }.unwrap_or(0);
            self.coded = ((packed >> 32) as u32, packed as u32);
            self.shown = aperture(&t).unwrap_or(self.coded);
            return true;
        }
        false
    }

    /// One access unit (Annex B, as received) decoded; each picture to `pictures`.
    fn decode(&mut self, unit: &[u8], pts: u64, rotation: u32, gpu: Option<&mut Gpu>, copier: &mut Copier, pictures: &Pictures) {
        let Some(sample) = memory_sample(unit, (pts as i64) * 1000 / 9) else { return };
        let mut gpu = gpu;
        for _ in 0..2 {
            match unsafe { self.mft.ProcessInput(0, &sample, 0) } {
                Ok(()) => break,
                // full: take what's done and try once more
                Err(e) if e.code() == MF_E_NOTACCEPTING => self.take(rotation, gpu.as_deref_mut(), copier, pictures),
                Err(_) => return,
            }
        }
        self.take(rotation, gpu, copier, pictures);
    }

    fn take(&mut self, rotation: u32, mut gpu: Option<&mut Gpu>, copier: &mut Copier, pictures: &Pictures) {
        let (mut coded, mut shown) = (self.coded, self.shown);
        let mut outs = Vec::new();
        drain(
            &self.mft,
            |mft| {
                let mut v = Video { hevc: self.hevc, params: Vec::new(), mft: mft.clone(), gpu: false, coded, shown };
                let ok = v.choose_output();
                (coded, shown) = (v.coded, v.shown);
                ok
            },
            |sample| outs.push(sample),
        );
        (self.coded, self.shown) = (coded, shown);
        for sample in outs {
            let pts = unsafe { sample.GetSampleTime() }.ok().map(|hns| (hns.max(0) as u64) * 9 / 1000);
            let picture = if self.gpu {
                gpu.as_deref_mut().and_then(|g| on_gpu(&sample, g, self.shown, rotation, copier))
            } else {
                in_memory(&sample, self.coded, self.shown, copier)
            };
            if let Some(buffer) = picture {
                pictures(&buffer, pts);
            }
        }
    }
}

impl Drop for Video {
    fn drop(&mut self) {
        unsafe {
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.mft.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }
    }
}

/// The part of the picture shown (its minimum display aperture), when the type says.
pub(crate) fn aperture(t: &IMFMediaType) -> Option<(u32, u32)> {
    let mut area = MFVideoArea::default();
    let mut size = 0u32;
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(&mut area as *mut MFVideoArea as *mut u8, size_of::<MFVideoArea>())
    };
    unsafe { t.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, bytes, Some(&mut size)) }.ok()?;
    (area.Area.cx > 0 && area.Area.cy > 0).then_some((area.Area.cx as u32 & !1, area.Area.cy as u32 & !1))
}

/// A decoded picture in a Direct3D texture, as a Lightair buffer.
pub(crate) fn on_gpu(sample: &IMFSample, gpu: &mut Gpu, shown: (u32, u32), rotation: u32, copier: &mut Copier) -> Option<Buffer> {
    let buffer = unsafe { sample.GetBufferByIndex(0) }.ok()?;
    let dxgi: IMFDXGIBuffer = buffer.cast().ok()?;
    let mut raw = std::ptr::null_mut();
    unsafe { dxgi.GetResource(&ID3D11Texture2D::IID, &mut raw) }.ok()?;
    // SAFETY: GetResource added a reference to the texture for us
    let texture = unsafe { ID3D11Texture2D::from_raw(raw) };
    let slice = unsafe { dxgi.GetSubresourceIndex() }.ok()?;
    // on the buffers' own device: converted straight into one of them
    if gpu.shares_buffers() {
        let out = gpu.out_size(&texture, shown, rotation)?;
        return copier.draw(out, |_, target| gpu.bgra_into(&texture, slice, shown, rotation, target));
    }
    let (handle, size) = gpu.bgra_of(&texture, slice, shown, rotation)?;
    copier.copy(handle, size, None)
}

/// A decoded picture in memory (NV12), as a Lightair buffer.
pub(crate) fn in_memory(sample: &IMFSample, coded: (u32, u32), shown: (u32, u32), copier: &mut Copier) -> Option<Buffer> {
    let buffer = unsafe { sample.ConvertToContiguousBuffer() }.ok()?;
    let mut at: *mut u8 = std::ptr::null_mut();
    let mut len = 0u32;
    unsafe { buffer.Lock(&mut at, None, Some(&mut len)) }.ok()?;
    let (cw, ch) = (coded.0 as usize, coded.1 as usize);
    let bgra = if (len as usize) >= cw * ch * 3 / 2 {
        // SAFETY: the locked buffer holds `len` bytes
        let data = unsafe { std::slice::from_raw_parts(at, len as usize) };
        Some(nv12_to_bgra(data, cw, ch, shown))
    } else {
        None
    };
    let _ = unsafe { buffer.Unlock() };
    let bgra = bgra?;
    copier.from_memory(&bgra, shown.0 as usize * 4, shown)
}

/// NV12 (`w` × `h`, rows `w` apart) to BGRA of `shown` size, BT.709 limited range.
fn nv12_to_bgra(data: &[u8], w: usize, h: usize, shown: (u32, u32)) -> Vec<u8> {
    let (sw, sh) = (shown.0 as usize, shown.1 as usize);
    let mut out = vec![0u8; sw * sh * 4];
    let uv = &data[w * h..];
    for y in 0..sh.min(h) {
        for x in 0..sw.min(w) {
            let yy = (data[y * w + x] as f32 - 16.0) * 1.164;
            let i = (y / 2) * w + (x & !1);
            let (u, v) = (uv[i] as f32 - 128.0, uv[i + 1] as f32 - 128.0);
            let r = yy + 1.793 * v;
            let g = yy - 0.213 * u - 0.533 * v;
            let b = yy + 2.112 * u;
            let o = (y * sw + x) * 4;
            out[o] = b.clamp(0.0, 255.0) as u8;
            out[o + 1] = g.clamp(0.0, 255.0) as u8;
            out[o + 2] = r.clamp(0.0, 255.0) as u8;
            out[o + 3] = 255;
        }
    }
    out
}

// ---------------------------------------------------------------------------------- sound

/// Media Foundation's AAC decoder for one rate and layout.
struct Audio {
    rate: u32,
    channels: u32,
    mft: IMFTransform,
    /// The decoder hands floats (else 16-bit samples).
    float: bool,
}

impl Audio {
    fn new(rate: u32, channels: u32) -> Option<Audio> {
        let mft = transform(MFT_CATEGORY_AUDIO_DECODER, MFMediaType_Audio, MFAudioFormat_AAC)?;
        let index = [96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000, 7_350]
            .iter()
            .position(|&r| r == rate)? as u16;
        // AAC-LC's AudioSpecificConfig after HEAACWAVEINFO's fields (raw frames, no ADTS)
        let config: u16 = (2 << 11) | (index << 7) | ((channels as u16) << 3);
        let mut user = vec![0u8; 12];
        user[2..4].copy_from_slice(&0xFEu16.to_le_bytes());
        user.extend_from_slice(&config.to_be_bytes());
        let float = unsafe {
            let input = MFCreateMediaType().ok()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio).ok()?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC).ok()?;
            input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate).ok()?;
            input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels).ok()?;
            input.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16).ok()?;
            input.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0).ok()?;
            input.SetBlob(&MF_MT_USER_DATA, &user).ok()?;
            mft.SetInputType(0, &input, 0).ok()?;
            // the decoder's own output types: floats if it offers them, else 16-bit samples
            let mut chosen = None;
            for i in 0..16 {
                let Ok(t) = mft.GetOutputAvailableType(0, i) else { break };
                let subtype = t.GetGUID(&MF_MT_SUBTYPE).ok();
                let bits = t.GetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE).unwrap_or(0);
                let is_float = subtype == Some(MFAudioFormat_Float);
                let is_pcm16 = subtype == Some(MFAudioFormat_PCM) && bits == 16;
                if is_float || (is_pcm16 && chosen.is_none()) {
                    chosen = Some((t, is_float));
                    if is_float {
                        break;
                    }
                }
            }
            let (output, float) = chosen?;
            mft.SetOutputType(0, &output, 0).ok()?;
            let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0);
            let _ = mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0);
            float
        };
        Some(Audio { rate, channels, mft, float })
    }

    /// One raw AAC frame decoded; its planes and rate to `out`.
    fn decode(&mut self, raw: &[u8], out: &mut dyn FnMut(&[&[f32]], u32)) {
        let Some(sample) = memory_sample(raw, 0) else { return };
        if unsafe { self.mft.ProcessInput(0, &sample, 0) }.is_err() {
            return;
        }
        let channels = self.channels.max(1) as usize;
        let (rate, float) = (self.rate, self.float);
        drain(
            &self.mft,
            |_| false,
            |sample| {
                let Ok(buffer) = (unsafe { sample.ConvertToContiguousBuffer() }) else { return };
                let mut at: *mut u8 = std::ptr::null_mut();
                let mut len = 0u32;
                if unsafe { buffer.Lock(&mut at, None, Some(&mut len)) }.is_err() {
                    return;
                }
                // SAFETY: the locked buffer holds `len` bytes of interleaved samples
                let samples: Vec<f32> = if float {
                    unsafe { std::slice::from_raw_parts(at as *const f32, len as usize / 4) }.to_vec()
                } else {
                    unsafe { std::slice::from_raw_parts(at as *const i16, len as usize / 2) }
                        .iter()
                        .map(|&v| v as f32 / 32_768.0)
                        .collect()
                };
                let frames = samples.len() / channels;
                let planes: Vec<Vec<f32>> =
                    (0..channels).map(|c| (0..frames).map(|f| samples[f * channels + c]).collect()).collect();
                let _ = unsafe { buffer.Unlock() };
                let views: Vec<&[f32]> = planes.iter().map(Vec::as_slice).collect();
                if frames > 0 {
                    out(&views, rate);
                }
            },
        );
    }
}

// ---------------------------------------------------------------------------------- the decoder

/// Whether `nal` is an access unit delimiter.
fn delimiter(hevc: bool, nal: &[u8]) -> bool {
    nal.first().is_some_and(|b| if hevc { (b >> 1) & 0x3F == 35 } else { b & 0x1F == 9 })
}

/// H.264 or HEVC and AAC decoded by Media Foundation, unit by unit as they come. Pictures go to
/// the [`Pictures`] it was made with, in decoding order; sound to the caller's function. Used on
/// one thread (made there: it joins COM's multithreaded apartment).
pub struct Decoder {
    video: Option<Video>,
    audio: Option<Audio>,
    pictures: Pictures,
    gpu: Option<Gpu>,
    copier: Copier,
    /// Said once that the sound couldn't be decoded.
    audio_failed: bool,
    /// Clockwise degrees the pictures are turned (a phone's, upright).
    rotation: u32,
    _mf: Option<Mf>,
}

// SAFETY: Media Foundation's objects here are used from the thread that owns the decoder.
unsafe impl Send for Decoder {}

impl Decoder {
    /// `bgra`: as on macOS (pictures are always BGRA Lightair buffers here).
    pub fn new(bgra: bool, pictures: Pictures) -> Decoder {
        let _ = bgra;
        let mf = Mf::start();
        let gpu = Gpu::new();
        if gpu.is_none() {
            tracing::info!("phone camera: decoding without the GPU's help (no Direct3D video device)");
        }
        Decoder { video: None, audio: None, pictures, gpu, copier: Copier::new(), audio_failed: false, rotation: 0, _mf: mf }
    }

    /// One video access unit (Annex B) and its PTS (and DTS) on the 90 kHz clock, decoded before
    /// this returns. A new set of parameter sets makes a new decoder; nothing is decoded before
    /// the first set.
    pub fn video(&mut self, hevc: bool, unit: &[u8], pts: u64, dts: Option<u64>) {
        let _ = dts;
        let all = nals(unit);
        let mut params: Vec<(u8, Vec<u8>)> =
            all.iter().filter_map(|n| parameter_set(hevc, n).map(|o| (o, n.to_vec()))).collect();
        params.sort_by_key(|(o, _)| *o);
        let params: Vec<Vec<u8>> = params.into_iter().map(|(_, p)| p).collect();
        let fresh = !params.is_empty() && self.video.as_ref().is_none_or(|v| v.hevc != hevc || v.params != params);
        if fresh {
            drop(self.video.take());
            self.video = Video::new(hevc, params, self.gpu.as_ref());
        }
        let Some(v) = self.video.as_mut() else { return };
        // (delimiters left out; parameter sets kept: the decoder reads them in band)
        let mut bytes = Vec::with_capacity(unit.len());
        for n in all.into_iter().filter(|n| !delimiter(hevc, n)) {
            bytes.extend_from_slice(&[0, 0, 0, 1]);
            bytes.extend_from_slice(n);
        }
        v.decode(&bytes, pts, self.rotation, self.gpu.as_mut(), &mut self.copier, &self.pictures);
    }

    /// Turns the pictures clockwise by `degrees` (0, 90, 180, 270) as they're converted (on the
    /// GPU; pictures decoded in memory aren't turned).
    pub fn set_rotation(&mut self, degrees: u32) {
        self.rotation = if matches!(degrees, 90 | 180 | 270) { degrees } else { 0 };
    }

    /// One unit of AAC in ADTS (one or more frames), decoded, each frame's planes and rate
    /// handed to `out` as it's decoded. Mono and stereo; other layouts are left out.
    pub fn sound(&mut self, unit: &[u8], out: &mut dyn FnMut(&[&[f32]], u32)) {
        for (rate, channels, raw) in adts(unit) {
            if channels > 2 {
                continue;
            }
            if self.audio.as_ref().is_none_or(|a| a.rate != rate || a.channels != channels) {
                self.audio = Audio::new(rate, channels);
                if self.audio.is_none() && !std::mem::replace(&mut self.audio_failed, true) {
                    tracing::warn!(rate, channels, "phone camera: Windows' AAC decoder didn't take the sound");
                }
            }
            if let Some(a) = self.audio.as_mut() {
                a.decode(raw, out);
            }
        }
    }
}
