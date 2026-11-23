//! PNGtubers (`.lairava`, lightair-core's `lightair-pngtuber`) drawn with libobs's graphics, as
//! the 2D avatars are ([`crate::avatar2d`]): each visible layer a textured quad, back to front,
//! where the runtime's pose puts it, into the avatar's texture. The pictures are straight alpha;
//! the output is premultiplied, see-through around the character.
//!
//! The runtime is driven by the mic: the level of a mixer track (the avatar's `mic` setting) from
//! the engine's meters.

use std::ffi::{CStr, CString, c_char, c_void};

use lightair_avatar::Framing;
use lightair_pngtuber::{Inputs, Puppet};

use crate::sys;

/// A PNGtuber read in the background, with its pictures' smaller copies made there too.
pub struct LoadedPng {
    pub puppet: Puppet,
    pub pictures: Vec<lightair_pngtuber::Picture>,
    pub mips: Vec<Vec<Vec<u8>>>,
    /// The mic's level (dBFS) and push to talk, for the next frame.
    level_db: f32,
    push_to_talk: Option<bool>,
}

impl LoadedPng {
    pub fn load(path: &std::path::Path) -> Result<LoadedPng, String> {
        let loaded = lightair_pngtuber::file::read_path(path).map_err(|e| e.to_string())?;
        let mips =
            loaded.pictures.iter().map(|p| lightair_avatar::vrm::mip_chain(p.width, p.height, &p.rgba)).collect();
        Ok(LoadedPng {
            puppet: Puppet::new(loaded.avatar),
            pictures: loaded.pictures,
            mips,
            level_db: f32::NEG_INFINITY,
            push_to_talk: None,
        })
    }

    pub fn set_mic(&mut self, level_db: f32, push_to_talk: Option<bool>) {
        self.level_db = level_db;
        self.push_to_talk = push_to_talk;
    }

    pub fn update(&mut self, expression: Option<&str>, dt: f32) {
        self.puppet.step(&Inputs { level_db: self.level_db, push_to_talk: self.push_to_talk, expression, dt });
    }

    /// The part of the canvas a framing shows, at an aspect (width / height): left, right, top,
    /// bottom (canvas pixels, y up, as the other flat avatars' frames).
    pub fn frame(&self, framing: Framing, aspect: f32, zoom: f32) -> (f32, f32, f32, f32) {
        let a = self.puppet.avatar();
        let (w, h) = (a.width as f32, a.height as f32);
        let share = match framing {
            Framing::Head => 0.5,
            Framing::Bust => 0.7,
            Framing::Half => 0.85,
            Framing::Full => 1.0,
        };
        let fh = h * share * 1.02 / zoom.clamp(0.25, 4.0);
        let top = h + h * 0.01;
        let fw = fh * aspect.max(0.05);
        (w * 0.5 - fw * 0.5, w * 0.5 + fw * 0.5, top, top - fh)
    }

    /// Its anchors then its layers, by name: what things can be pinned to.
    pub fn anchor_names(&self) -> Vec<(String, String)> {
        let a = self.puppet.avatar();
        a.anchors
            .iter()
            .map(|x| (x.name.clone(), "Anchors".to_string()))
            .chain(a.layers.iter().map(|l| (l.name.clone(), "Layers".to_string())))
            .collect()
    }

    /// An anchor's or a layer's points this frame (canvas pixels, y up): a little cross at an
    /// anchor, a layer's corners.
    pub fn anchor_points(&self, i: usize) -> Vec<[f32; 2]> {
        let a = self.puppet.avatar();
        let h = a.height as f32;
        let up = |p: [f32; 2]| [p[0], h - p[1]];
        let n = a.anchors.len();
        if i < n {
            let x = &a.anchors[i];
            let pose = x
                .layer
                .as_ref()
                .and_then(|id| a.layers.iter().position(|l| l.id == *id))
                .or((!a.layers.is_empty()).then_some(0))
                .and_then(|j| self.puppet.pose().layers.get(j).copied());
            let place = |p: [f32; 2]| pose.map_or(p, |q| q.place(p));
            vec![up(place([x.x, x.y])), up(place([x.x + 12.0, x.y])), up(place([x.x, x.y + 12.0]))]
        } else {
            self.puppet.layer_corners(i - n).map(|c| c.iter().map(|p| up(*p)).collect()).unwrap_or_default()
        }
    }
}

const EFFECT: &str = r#"
uniform float4x4 ViewProj;
uniform texture2d image;
uniform float opacity;

sampler_state tex_sampler {
    Filter = Linear;
    AddressU = Clamp;
    AddressV = Clamp;
};

struct VertInOut {
    float4 pos : POSITION;
    float2 uv : TEXCOORD0;
};

VertInOut VSDefault(VertInOut v)
{
    VertInOut o;
    o.pos = mul(float4(v.pos.xyz, 1.0), ViewProj);
    o.uv = v.uv;
    return o;
}

float4 PSDraw(VertInOut v) : TARGET
{
    float4 c = image.Sample(tex_sampler, v.uv);
    c.a *= opacity;
    return float4(c.rgb * c.a, c.a);
}

technique Draw
{
    pass
    {
        vertex_shader = VSDefault(v);
        pixel_shader = PSDraw(v);
    }
}
"#;

/// A PNGtuber on the GPU: a texture per layer.
pub struct GpuPng {
    pub path: String,
    textures: Vec<*mut sys::gs_texture_t>,
    effect: *mut sys::gs_effect_t,
}

impl GpuPng {
    /// (Graphics context held.)
    pub unsafe fn new(path: &str, l: &LoadedPng) -> GpuPng {
        let mut textures = Vec::new();
        for (p, mips) in l.pictures.iter().zip(&l.mips) {
            let data: Vec<*const u8> =
                std::iter::once(p.rgba.as_ptr()).chain(mips.iter().map(|m| m.as_ptr())).collect();
            textures.push(unsafe {
                sys::gs_texture_create(
                    p.width,
                    p.height,
                    sys::GS_RGBA,
                    data.len() as u32,
                    data.as_ptr() as *mut *const u8,
                    0,
                )
            });
        }
        let code = CString::new(EFFECT).unwrap_or_default();
        let mut err: *mut c_char = std::ptr::null_mut();
        let effect = unsafe { sys::gs_effect_create(code.as_ptr(), c"lightair-pngtuber.effect".as_ptr(), &mut err) };
        if !err.is_null() {
            tracing::warn!(error = %unsafe { CStr::from_ptr(err) }.to_string_lossy(), "PNGtuber shader");
            unsafe { sys::bfree(err as *mut c_void) };
        }
        GpuPng { path: path.to_string(), textures, effect }
    }

    /// Draws the posed layers into `target` (`w`×`h`, cleared first). (Graphics context held.)
    pub unsafe fn draw(
        &self,
        l: &LoadedPng,
        target: *mut sys::gs_texrender_t,
        w: u32,
        h: u32,
        framing: Framing,
        zoom: f32,
    ) {
        if self.effect.is_null() || target.is_null() {
            return;
        }
        let a = l.puppet.avatar();
        let ch = a.height as f32;
        let (left, right, top, bottom) = l.frame(framing, w as f32 / h as f32, zoom);
        let e = self.effect;
        let param = |n: &CStr| unsafe { sys::gs_effect_get_param_by_name(e, n.as_ptr()) };
        unsafe {
            sys::gs_texrender_reset(target);
            if !sys::gs_texrender_begin(target, w, h) {
                return;
            }
            let clear = sys::vec4 { ..std::mem::zeroed() };
            sys::gs_clear(sys::GS_CLEAR_COLOR, &clear, 1.0, 0);
            // the canvas y down: the frame's top edge at the picture's top
            sys::gs_ortho(left, right, ch - top, ch - bottom, -100.0, 100.0);
            let cull = sys::gs_get_cull_mode();
            sys::gs_set_cull_mode(sys::GS_NEITHER);
            sys::gs_blend_state_push();
            sys::gs_enable_blending(true);
            sys::gs_blend_function_separate(
                sys::GS_BLEND_ONE,
                sys::GS_BLEND_INVSRCALPHA,
                sys::GS_BLEND_ONE,
                sys::GS_BLEND_INVSRCALPHA,
            );
            let pose = l.puppet.pose();
            for (i, layer) in a.layers.iter().enumerate() {
                let (Some(p), Some(&tex)) = (pose.layers.get(i), self.textures.get(i)) else { continue };
                if !p.visible || p.opacity <= 0.001 || tex.is_null() {
                    continue;
                }
                let [x, y] = p.place([layer.x, layer.y]);
                let (lw, lh) = (layer.width * p.sx, layer.height * p.sy);
                let (tw, th) = (sys::gs_texture_get_width(tex) as f32, sys::gs_texture_get_height(tex) as f32);
                sys::gs_matrix_push();
                sys::gs_matrix_translate3f(x, y, 0.0);
                sys::gs_matrix_scale3f(lw / tw.max(1.0), lh / th.max(1.0), 1.0);
                sys::gs_effect_set_texture(param(c"image"), tex);
                sys::gs_effect_set_float(param(c"opacity"), p.opacity);
                while sys::gs_effect_loop(e, c"Draw".as_ptr()) {
                    sys::gs_draw_sprite(tex, 0, 0, 0);
                }
                sys::gs_matrix_pop();
            }
            sys::gs_set_cull_mode(cull);
            sys::gs_blend_state_pop();
            sys::gs_texrender_end(target);
        }
    }

    /// (Graphics context held.)
    pub unsafe fn destroy(&mut self) {
        unsafe {
            for t in self.textures.drain(..) {
                if !t.is_null() {
                    sys::gs_texture_destroy(t);
                }
            }
            if !self.effect.is_null() {
                sys::gs_effect_destroy(self.effect);
                self.effect = std::ptr::null_mut();
            }
        }
    }
}
