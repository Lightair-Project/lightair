//! The canvas preview drawn by DirectComposition, under the window's UI (Windows).
//!
//! GPUI shows its window through a DirectComposition swap chain with alpha; the patched GPUI
//! (third_party/gpui-pre-windows) puts an "underlay" visual below it. Here the underlay holds the
//! window's background, the canvas well, and in the well one visual per canvas showing a swap
//! chain that libobs's graphics thread presents the preview to (`lightair_preview::Layer`). GPUI
//! leaves the well transparent, so the picture shows through, and a new frame redraws nothing
//! of the UI: the counterpart of the macOS Core Animation preview.

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::{Bounds, Hsla, Pixels, Window};
use lightair_core::Id;
use lightair_preview::Layer;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::Foundation::POINT;
use windows::Win32::Graphics::Direct2D::Common::D2D_RECT_F;
use windows::Win32::Graphics::Direct3D11::{D3D11_BOX, ID3D11Texture2D};
use windows::Win32::Graphics::DirectComposition::{
    DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR, DCOMPOSITION_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
    DCOMPOSITION_BORDER_MODE_HARD, IDCompositionDevice, IDCompositionSurface, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_B8G8R8A8_UNORM};
use windows::core::Interface;
use windows_numerics::Matrix3x2;

/// The well and each canvas's picture, as last placed.
type Placed = (Option<Bounds<Pixels>>, Vec<(Id, Bounds<Pixels>)>);

/// A colour filling its visual: a 1×1 surface stretched by the visual's transform.
struct Solid {
    visual: IDCompositionVisual,
    _surface: IDCompositionSurface,
}

impl Solid {
    fn new(device: &IDCompositionDevice, colour: Hsla) -> windows::core::Result<Self> {
        unsafe {
            let visual = device.CreateVisual()?;
            let surface = device.CreateSurface(1, 1, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_ALPHA_MODE_IGNORE)?;
            let mut at = POINT::default();
            let tex: ID3D11Texture2D = surface.BeginDraw(None, &mut at)?;
            let c = colour.to_rgb();
            let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            let pixel = [q(c.b), q(c.g), q(c.r), 255u8];
            let (x, y) = (at.x as u32, at.y as u32);
            // the surface may be a part of a larger texture: only its pixel is written
            let place = D3D11_BOX { left: x, top: y, front: 0, right: x + 1, bottom: y + 1, back: 1 };
            let context = tex.GetDevice()?.GetImmediateContext()?;
            context.UpdateSubresource(&tex, 0, Some(&place), pixel.as_ptr().cast(), 4, 4);
            drop(tex);
            surface.EndDraw()?;
            visual.SetContent(&surface)?;
            visual.SetBitmapInterpolationMode(DCOMPOSITION_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR)?;
            visual.SetBorderMode(DCOMPOSITION_BORDER_MODE_HARD)?;
            Ok(Self { visual, _surface: surface })
        }
    }
}

fn transform(visual: &IDCompositionVisual, sx: f32, sy: f32, x: f32, y: f32) {
    let m = Matrix3x2 { M11: sx, M12: 0.0, M21: 0.0, M22: sy, M31: x, M32: y };
    unsafe {
        let _ = visual.SetTransform2(&m);
    }
}

fn clip(visual: &IDCompositionVisual, w: f32, h: f32) {
    let r = D2D_RECT_F { left: 0.0, top: 0.0, right: w.max(0.0), bottom: h.max(0.0) };
    unsafe {
        let _ = visual.SetClip2(&r);
    }
}

struct Picture {
    visual: IDCompositionVisual,
    layer: Arc<Layer>,
    /// The swap chain's size its transform was worked out for.
    sized: (u32, u32),
}

/// The visuals under the UI: the background, the well (clipping its pictures) and each canvas's
/// picture.
pub struct NativePreview {
    hwnd: isize,
    generation: u64,
    device: IDCompositionDevice,
    underlay: IDCompositionVisual,
    _background: Solid,
    well: IDCompositionVisual,
    well_fill: Solid,
    pictures: HashMap<Id, Picture>,
    last: Placed,
    visible: bool,
    scale: f32,
}

impl NativePreview {
    /// Fills the window's underlay (GPUI's window must be transparent), painting `background`
    /// everywhere and `well` where the canvas well is.
    pub fn attach(window: &Window, background: Hsla, well: Hsla) -> Option<Self> {
        let handle = HasWindowHandle::window_handle(window).ok()?;
        let RawWindowHandle::Win32(h) = handle.as_raw() else { return None };
        let hwnd = h.hwnd.get();
        let (device, underlay, generation) = gpui_windows::underlay::get(hwnd)?;
        // SAFETY: `get` added a reference to each for us.
        let (device, underlay) = unsafe { (IDCompositionDevice::from_raw(device), IDCompositionVisual::from_raw(underlay)) };
        let made = (|| -> windows::core::Result<_> {
            let bg = Solid::new(&device, background)?;
            // larger than any window; the window clips it
            transform(&bg.visual, 32768.0, 32768.0, 0.0, 0.0);
            let well_visual = unsafe { device.CreateVisual()? };
            let fill = Solid::new(&device, well)?;
            unsafe {
                underlay.RemoveAllVisuals()?;
                underlay.AddVisual(&bg.visual, false, None)?;
                well_visual.AddVisual(&fill.visual, false, None)?;
                underlay.AddVisual(&well_visual, true, &bg.visual)?;
            }
            clip(&well_visual, 0.0, 0.0);
            unsafe { device.Commit()? };
            Ok((bg, well_visual, fill))
        })();
        let (bg, well_visual, fill) = match made {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("the preview under the UI couldn't be made: {e}");
                return None;
            }
        };
        Some(Self {
            hwnd,
            generation,
            device,
            underlay,
            _background: bg,
            well: well_visual,
            well_fill: fill,
            pictures: HashMap::new(),
            last: (None, vec![]),
            visible: true,
            scale: window.scale_factor(),
        })
    }

    /// Whether GPUI still shows these visuals (it remakes its composition after a lost device).
    pub fn current(&self) -> bool {
        gpui_windows::underlay::generation(self.hwnd) == self.generation
    }

    /// The layer a canvas's preview presents its frames to (made the first time), for
    /// `Preview::set_layer`; null if it couldn't be made.
    pub fn picture(&mut self, canvas: Id) -> *mut std::ffi::c_void {
        if let Some(p) = self.pictures.get(&canvas) {
            return Arc::as_ptr(&p.layer) as *mut _;
        }
        let Some(layer) = Layer::new() else { return std::ptr::null_mut() };
        let layer = Arc::new(layer);
        let made = (|| -> windows::core::Result<IDCompositionVisual> {
            unsafe {
                let visual = self.device.CreateVisual()?;
                let swap = layer.swap_chain();
                let swap = windows::core::IUnknown::from_raw_borrowed(&swap);
                visual.SetContent(swap)?;
                visual.SetBitmapInterpolationMode(DCOMPOSITION_BITMAP_INTERPOLATION_MODE_LINEAR)?;
                // hidden until placed
                transform(&visual, 0.0, 0.0, 0.0, 0.0);
                self.well.AddVisual(&visual, true, &self.well_fill.visual)?;
                self.device.Commit()?;
                Ok(visual)
            }
        })();
        match made {
            Ok(visual) => {
                let ptr = Arc::as_ptr(&layer) as *mut _;
                self.pictures.insert(canvas, Picture { visual, layer, sized: (0, 0) });
                ptr
            }
            Err(e) => {
                tracing::warn!("canvas picture visual: {e}");
                std::ptr::null_mut()
            }
        }
    }

    /// Where the well is (window points), and each shown canvas's picture in it; `None`: not
    /// on screen (another place, the Library, Editor).
    pub fn place(&mut self, well: Option<Bounds<Pixels>>, pictures: &[(Id, Bounds<Pixels>)]) {
        let resized = self.pictures.values().any(|p| p.layer.size() != p.sized);
        if !resized && self.last.0 == well && self.last.1 == pictures {
            return;
        }
        self.last = (well, pictures.to_vec());
        self.apply();
    }

    fn apply(&mut self) {
        let s = self.scale;
        let px = |v: Pixels| f32::from(v) * s;
        match self.last.0.filter(|_| self.visible) {
            None => clip(&self.well, 0.0, 0.0),
            Some(w) => {
                let (wx, wy) = (px(w.origin.x).round(), px(w.origin.y).round());
                let (ww, wh) = (px(w.size.width).round(), px(w.size.height).round());
                transform(&self.well, 1.0, 1.0, wx, wy);
                clip(&self.well, ww, wh);
                transform(&self.well_fill.visual, ww, wh, 0.0, 0.0);
                for (id, p) in &mut self.pictures {
                    p.sized = p.layer.size();
                    match self.last.1.iter().find(|(c, _)| c == id) {
                        Some((_, b)) => {
                            let (sw, sh) = p.sized;
                            let (x, y) = ((px(b.origin.x) - wx).round(), (px(b.origin.y) - wy).round());
                            let (bw, bh) = (px(b.size.width).round(), px(b.size.height).round());
                            transform(&p.visual, bw / sw.max(1) as f32, bh / sh.max(1) as f32, x, y);
                        }
                        None => transform(&p.visual, 0.0, 0.0, 0.0, 0.0),
                    }
                }
            }
        }
        unsafe {
            let _ = self.device.Commit();
        }
    }

    /// The canvas is on screen (the Live place) or not; placed again as it last was.
    pub fn set_visible(&mut self, visible: bool) {
        if visible != self.visible {
            self.visible = visible;
            self.apply();
        }
    }

    /// A canvas gone: its visual and swap chain go too.
    pub fn forget(&mut self, canvas: Id) {
        if let Some(p) = self.pictures.remove(&canvas) {
            unsafe {
                let _ = self.well.RemoveVisual(&p.visual);
                let _ = self.device.Commit();
            }
        }
    }

    /// The meters are drawn by GPUI on Windows.
    pub fn place_meters(&mut self, _: &[(Id, super::MeterPlace)]) {}
    pub fn show_meters(&mut self, _: bool) {}
}

impl Drop for NativePreview {
    fn drop(&mut self) {
        unsafe {
            let _ = self.underlay.RemoveAllVisuals();
            let _ = self.device.Commit();
        }
    }
}
