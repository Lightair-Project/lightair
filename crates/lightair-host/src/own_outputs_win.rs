//! Windows: the compositor's frames into the GPU's Media Foundation encoders and the taps.
//!
//! The compositor's buffers (`crate::buffers`: BGRA Direct3D 11 textures with NT shared
//! handles) open on one video device per graphics adapter, the buffers' own adapter
//! (`lightair_encode::windows::video_device`), where every encoder and tap of Lightair's outputs
//! works. An encoder copies each frame into one of its own input textures on the GPU (scaled
//! when its size differs) and waits for the copy, so the compositor's buffer is free again at
//! once. A tap copies each frame into a texture of its own (scaled, or converted to NV12) and
//! reads it back into memory on its own thread, holding the compositor's buffer until the copy
//! is done.

use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use lightair_encode::Color;
use lightair_encode::windows::{Blit, Options, RateControl, VideoEncoder, VideoSettings, VideoSink, video_device};
use parking_lot::Mutex;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ, D3D11_MAP_FLAG_DO_NOT_WAIT,
    D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
    ID3D11Device, ID3D11Device1, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::DXGI_ERROR_WAS_STILL_DRAWING;
use windows::core::Interface;

use super::Buffer;
use crate::buffers::Shared;

/// A tap's picture in memory, for as long as the tap's function runs.
pub struct Picture<'a> {
    pub width: u32,
    pub height: u32,
    /// Bytes from one row to the next (in both of NV12's planes).
    pub stride: usize,
    pub format: TapFormat,
    /// BGRA: `height` rows. NV12: `height` rows of luma, then `height / 2` rows of Cb and Cr
    /// side by side.
    pub data: &'a [u8],
}

/// What a tap does with each picture (its own copy of the frame, and its time), on the tap's
/// own thread.
pub type TapFn = Box<dyn FnMut(&Picture<'_>, i64) + Send>;

/// The pictures a tap takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TapFormat {
    Bgra,
    /// NV12 in BT.601's matrix, limited range: what apps take a webcam's NV12 to be.
    Nv12Bt601,
}

/// BT.601, limited range.
const BT601: Color = Color { primaries: 6, transfer: 6, matrix: 6, full_range: false };

/// Input textures each encoder copies frames into (B-frames keep a few inside the encoder).
const ENCODER_SLOTS: u32 = 6;
/// Textures each tap copies frames into in turn: the newest, one on its way to the tap's
/// thread, one being read there, one to write.
const TAP_DEPTH: usize = 4;
/// How long a tap waits for its copy on the GPU.
const READBACK_WAIT: Duration = Duration::from_millis(250);

// ---------------------------------------------------------------- the device

/// The device the encoders and taps work on, and the buffers' device it was made beside.
struct Device {
    buffers: ID3D11Device,
    ours: ID3D11Device,
}

// SAFETY: Direct3D 11 devices made multithread-protected are free-threaded.
unsafe impl Send for Device {}

static DEVICE: Mutex<Option<Device>> = Mutex::new(None);

/// The video device on the buffers' graphics adapter, made once per adapter.
fn device() -> Result<ID3D11Device, String> {
    let buffers = crate::buffers::device().ok_or_else(|| "There's no graphics card to encode on.".to_string())?;
    let mut held = DEVICE.lock();
    if let Some(d) = held.as_ref()
        && d.buffers == buffers
    {
        return Ok(d.ours.clone());
    }
    let ours = video_device(Some(&buffers)).map_err(|e| format!("The graphics card can't encode ({e})."))?;
    *held = Some(Device { buffers, ours: ours.clone() });
    Ok(ours)
}

/// The compositor's buffers opened on one device, by buffer (a buffer is opened once, and let
/// go once it's gone).
struct Opened {
    device: ID3D11Device,
    list: Vec<(Weak<Shared>, ID3D11Texture2D)>,
    /// A buffer didn't open (another graphics adapter): said once.
    failed: bool,
}

impl Opened {
    fn new(device: ID3D11Device) -> Opened {
        Opened { device, list: Vec::new(), failed: false }
    }

    /// `buffer`'s texture on this device.
    fn get(&mut self, buffer: &Buffer) -> Option<ID3D11Texture2D> {
        self.list.retain(|(b, _)| b.strong_count() > 0);
        if let Some((_, t)) = self.list.iter().find(|(b, _)| std::ptr::eq(b.as_ptr(), Arc::as_ptr(buffer))) {
            return Some(t.clone());
        }
        let opened = self
            .device
            .cast::<ID3D11Device1>()
            .and_then(|d| unsafe { d.OpenSharedResource1::<ID3D11Texture2D>(buffer.handle()) });
        match opened {
            Ok(texture) => {
                self.list.push((Arc::downgrade(buffer), texture.clone()));
                Some(texture)
            }
            Err(e) => {
                if !std::mem::replace(&mut self.failed, true) {
                    tracing::warn!(%e, "own outputs: a compositor picture didn't open for the encoders");
                }
                None
            }
        }
    }
}

// ---------------------------------------------------------------- encoders

/// The GPU's encoder on the outputs' device: constant bitrate when `cbr`, else peak-constrained
/// variable bitrate.
pub fn open_encoder(
    settings: VideoSettings,
    cbr: bool,
    sink: VideoSink,
) -> Result<VideoEncoder, lightair_encode::Error> {
    let device = device().map_err(lightair_encode::Error::Unsupported)?;
    let options = Options {
        device: Some(device),
        rate_control: Some(if cbr { RateControl::Cbr } else { RateControl::Vbr }),
        pool_size: ENCODER_SLOTS,
        vendor: None,
    };
    VideoEncoder::with_options(settings, options, sink)
}

/// What became of a frame given to an encoder.
pub enum Fed {
    /// It couldn't be copied.
    NotCopied,
    /// No picture yet (nothing to repeat).
    NoPicture,
    Encoded(Result<(), lightair_encode::Error>),
}

/// The compositor's buffers opened on an encoder's device (the encoder keeps its last picture
/// itself).
pub struct Copier {
    opened: Option<Opened>,
}

// SAFETY: used under its mutex only; Direct3D 11 textures are free-threaded.
unsafe impl Send for Copier {}

impl Copier {
    /// `fast`: the plan's point scaler (Windows' video processor scales one way only).
    pub fn new(fast: bool) -> Copier {
        let _ = fast;
        Copier { opened: None }
    }

    /// Copies `frame` into one of `encoder`'s input textures and encodes it (`None`: the last
    /// picture again).
    pub fn feed(&mut self, encoder: &VideoEncoder, frame: Option<&Buffer>, pts: i64, key: bool) -> Fed {
        let source = match frame {
            Some(buffer) => {
                let opened = self.opened.get_or_insert_with(|| Opened::new(encoder.device().clone()));
                match opened.get(buffer) {
                    Some(texture) => Some(texture),
                    None => return Fed::NotCopied,
                }
            }
            None => None,
        };
        match encoder.encode_copy(source.as_ref(), pts, key) {
            Ok(true) => Fed::Encoded(Ok(())),
            Ok(false) => Fed::NoPicture,
            Err(e) => Fed::Encoded(Err(e)),
        }
    }
}

// ---------------------------------------------------------------- taps

fn texture(
    device: &ID3D11Device,
    (w, h): (u32, u32),
    format: DXGI_FORMAT,
    staging: bool,
) -> Result<ID3D11Texture2D, String> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: w,
        Height: h,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: if staging { D3D11_USAGE_STAGING } else { D3D11_USAGE_DEFAULT },
        BindFlags: if staging { 0 } else { (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32 },
        CPUAccessFlags: if staging { D3D11_CPU_ACCESS_READ.0 as u32 } else { 0 },
        MiscFlags: 0,
    };
    let mut out = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut out)) }
        .map_err(|e| format!("Couldn't make pictures to send ({e})."))?;
    out.ok_or_else(|| "Couldn't make pictures to send.".to_string())
}

/// One of a tap's copies: the texture the GPU draws it into (when it's scaled or converted),
/// the texture it's read back from, and the compositor's buffer it's copied from until the
/// copy is done.
struct Staged {
    target: ID3D11Texture2D,
    staging: ID3D11Texture2D,
    context: ID3D11DeviceContext,
    hold: Mutex<Option<Buffer>>,
    size: (u32, u32),
    format: TapFormat,
}

// SAFETY: Direct3D 11 textures and contexts of a multithread-protected device are free-threaded.
unsafe impl Send for Staged {}
unsafe impl Sync for Staged {}

/// Copies the compositor's frames for a tap, into its textures in turn.
pub struct TapCopier {
    blit: Blit,
    opened: Opened,
    context: ID3D11DeviceContext,
    /// A copy is written again only when nothing else holds it (the tap's last picture, one on
    /// its way to the tap's thread).
    ring: Vec<Arc<Staged>>,
    next: usize,
}

// SAFETY: used under its tap's mutex only; see `Staged`.
unsafe impl Send for TapCopier {}

impl TapCopier {
    pub fn new(width: u32, height: u32, format: TapFormat) -> Result<TapCopier, String> {
        let device = device()?;
        let blit = Blit::new(&device).map_err(|e| format!("Couldn't copy the canvas's pictures ({e})."))?;
        let context = unsafe { device.GetImmediateContext() }
            .map_err(|e| format!("Couldn't copy the canvas's pictures ({e})."))?;
        let dxgi = match format {
            TapFormat::Bgra => DXGI_FORMAT_B8G8R8A8_UNORM,
            TapFormat::Nv12Bt601 => DXGI_FORMAT_NV12,
        };
        let size = (width, height);
        let mut ring = Vec::with_capacity(TAP_DEPTH);
        for _ in 0..TAP_DEPTH {
            ring.push(Arc::new(Staged {
                target: texture(&device, size, dxgi, false)?,
                staging: texture(&device, size, dxgi, true)?,
                context: context.clone(),
                hold: Mutex::new(None),
                size,
                format,
            }));
        }
        Ok(TapCopier { blit, opened: Opened::new(device), context, ring, next: 0 })
    }

    /// `src` copied for reading back; `None` when it couldn't be (or every copy is still held).
    pub fn copy(&mut self, src: &Buffer) -> Option<TapPicture> {
        let texture = self.opened.get(src)?;
        let n = self.ring.len();
        let i = (0..n).map(|k| (self.next + k) % n).find(|&i| Arc::strong_count(&self.ring[i]) == 1)?;
        self.next = (i + 1) % n;
        let staged = &self.ring[i];
        if staged.format == TapFormat::Bgra && src.size() == staged.size {
            unsafe { self.context.CopyResource(&staged.staging, &texture) };
        } else {
            if let Err(e) = self.blit.copy(&texture, &staged.target, &BT601) {
                tracing::debug!(%e, "own outputs: a picture wasn't copied for a tap");
                return None;
            }
            unsafe { self.context.CopyResource(&staged.staging, &staged.target) };
        }
        unsafe { self.context.Flush() };
        *staged.hold.lock() = Some(src.clone());
        Some(TapPicture(staged.clone()))
    }
}

/// A tap's copy of a frame, on its way to the tap's thread.
#[derive(Clone)]
pub struct TapPicture(Arc<Staged>);

impl TapPicture {
    /// Reads the picture back (once the GPU has copied it) and gives it to the tap's function.
    /// The tap's thread.
    pub fn give(&self, f: &mut TapFn, pts: i64) {
        let s = &self.0;
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // asked again until the copy is done, so the device isn't held while the GPU works
        let deadline = Instant::now() + READBACK_WAIT;
        let result = loop {
            let r = unsafe {
                s.context.Map(&s.staging, 0, D3D11_MAP_READ, D3D11_MAP_FLAG_DO_NOT_WAIT.0 as u32, Some(&mut mapped))
            };
            match r {
                Err(e) if e.code() == DXGI_ERROR_WAS_STILL_DRAWING && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(1));
                }
                other => break other,
            }
        };
        // the copy is done (or given up): the compositor may draw into its buffer again
        s.hold.lock().take();
        if let Err(e) = result {
            tracing::debug!(%e, "own outputs: a tap's picture wasn't read back");
            return;
        }
        let (w, h) = s.size;
        let rows = match s.format {
            TapFormat::Bgra => h as usize,
            TapFormat::Nv12Bt601 => h as usize + h as usize / 2,
        };
        let stride = mapped.RowPitch as usize;
        // SAFETY: a mapped staging texture is `rows` rows of `RowPitch` bytes (NV12's chroma
        // right after its luma), readable until it's unmapped below.
        let data = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, stride * rows) };
        f(&Picture { width: w, height: h, stride, format: s.format, data }, pts);
        unsafe { s.context.Unmap(&s.staging, 0) };
    }
}
