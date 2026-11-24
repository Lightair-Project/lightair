//! Lightair's own video filters for the effect catalog (lightair-model's `catalog`, docs/13
//! "Built-in replacements"): a mask made from another source's picture, a freeze frame, and a
//! user shader that takes OBS Shaderfilter's files as they are. Lightair's own code.
//!
//! - **Object mask** (`lightair_source_mask`): `source` (a source's id, the libobs source's
//!   name), `mode` (0 its alpha, 1 its brightness), `invert`. The mask source is drawn at this
//!   source's size each frame and kept showing while the filter uses it.
//! - **Freeze** (`lightair_freeze`): `frozen` holds the picture from the moment it's switched
//!   on; `hold_ms` thaws it after that long (0: until switched off); `fade` lets that much of the
//!   live picture through.
//! - **Custom shader** (`lightair_shader`): `path` to a `.shader`/`.effect` file. A file with no
//!   `technique` is a Shaderfilter shader: it gets Shaderfilter's uniforms (`image`,
//!   `elapsed_time`, `uv_size`, `uv_pixel_interval`, `uv_scale`, `uv_offset`, `rand_f`, …), its
//!   `mainImage` runs, and `amount` mixes it with the picture.

use std::ffi::{CStr, CString, c_char, c_void};
use std::time::Instant;

use crate::sys;

unsafe fn get_str(settings: *mut sys::obs_data_t, key: &CStr) -> String {
    let p = unsafe { sys::obs_data_get_string(settings, key.as_ptr()) };
    if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
}

unsafe fn target_size(context: *mut sys::obs_source_t) -> (u32, u32) {
    let target = unsafe { sys::obs_filter_get_target(context) };
    if target.is_null() {
        (0, 0)
    } else {
        unsafe { (sys::obs_source_get_base_width(target), sys::obs_source_get_base_height(target)) }
    }
}

unsafe fn make_effect(code: &str, name: &CStr) -> *mut sys::gs_effect_t {
    let Ok(code) = CString::new(code) else { return std::ptr::null_mut() };
    let mut err: *mut c_char = std::ptr::null_mut();
    let e = unsafe { sys::gs_effect_create(code.as_ptr(), name.as_ptr(), &mut err) };
    if !err.is_null() {
        tracing::warn!(error = %unsafe { CStr::from_ptr(err) }.to_string_lossy(), effect = %name.to_string_lossy(), "shader didn't compile");
        unsafe { sys::bfree(err as *mut c_void) };
    }
    e
}

unsafe fn destroy_effect(e: &mut *mut sys::gs_effect_t) {
    if !e.is_null() {
        unsafe { sys::gs_effect_destroy(*e) };
        *e = std::ptr::null_mut();
    }
}

unsafe fn param(e: *mut sys::gs_effect_t, name: &CStr) -> *mut sys::gs_eparam_t {
    unsafe { sys::gs_effect_get_param_by_name(e, name.as_ptr()) }
}

fn vec2(x: f32, y: f32) -> sys::vec2 {
    sys::vec2 { __bindgen_anon_1: sys::vec2__bindgen_ty_1 { ptr: [x, y] } }
}

const VERTEX: &str = r#"
uniform float4x4 ViewProj;
struct VertInOut { float4 pos : POSITION; float2 uv : TEXCOORD0; };
VertInOut VSDefault(VertInOut vert_in)
{
    VertInOut vert_out;
    vert_out.pos = mul(float4(vert_in.pos.xyz, 1.0), ViewProj);
    vert_out.uv = vert_in.uv;
    return vert_out;
}
"#;

// ---------------------------------------------------------------------------------------------
// Object mask

const MASK_EFFECT: &str = r#"
uniform texture2d image;
uniform texture2d mask;
uniform float mode;
uniform float invert;
sampler_state def { Filter = Linear; AddressU = Clamp; AddressV = Clamp; };
float4 PSMask(VertInOut vert_in) : TARGET
{
    float4 c = image.Sample(def, vert_in.uv);
    float4 m = mask.Sample(def, vert_in.uv);
    float a = mode < 0.5 ? m.a : saturate(dot(m.rgb, float3(0.2126, 0.7152, 0.0722)) * m.a);
    if (invert > 0.5) a = 1.0 - a;
    return float4(c.rgb, c.a * a);
}
technique Draw { pass { vertex_shader = VSDefault(vert_in); pixel_shader = PSMask(vert_in); } }
"#;

struct SourceMask {
    context: *mut sys::obs_source_t,
    effect: *mut sys::gs_effect_t,
    texrender: *mut sys::gs_texrender_t,
    name: String,
    mode: f32,
    invert: bool,
    /// The mask source held showing, and its name then.
    held: Option<(*mut sys::obs_source_t, String)>,
}

impl SourceMask {
    /// The mask source for the current name, kept showing (the one before released).
    unsafe fn mask_source(&mut self) -> *mut sys::obs_source_t {
        if self.held.as_ref().is_some_and(|(_, n)| *n != self.name) {
            unsafe { self.release() };
        }
        if self.held.is_none() && !self.name.is_empty() {
            let Ok(n) = CString::new(self.name.clone()) else { return std::ptr::null_mut() };
            let s = unsafe { sys::obs_get_source_by_name(n.as_ptr()) };
            // never itself (it would draw forever)
            if !s.is_null() && s == unsafe { sys::obs_filter_get_parent(self.context) } {
                unsafe { sys::obs_source_release(s) };
                return std::ptr::null_mut();
            }
            if !s.is_null() {
                unsafe { sys::obs_source_inc_showing(s) };
                self.held = Some((s, self.name.clone()));
            }
        }
        self.held.as_ref().map_or(std::ptr::null_mut(), |(s, _)| *s)
    }

    unsafe fn release(&mut self) {
        if let Some((s, _)) = self.held.take() {
            unsafe {
                sys::obs_source_dec_showing(s);
                sys::obs_source_release(s);
            }
        }
    }
}

unsafe extern "C" fn mask_name(_: *mut c_void) -> *const c_char {
    c"Object mask".as_ptr()
}

unsafe extern "C" fn mask_update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    let f = unsafe { &mut *(data as *mut SourceMask) };
    f.name = unsafe { get_str(settings, c"source") };
    f.mode = unsafe { sys::obs_data_get_int(settings, c"mode".as_ptr()) } as f32;
    f.invert = unsafe { sys::obs_data_get_bool(settings, c"invert".as_ptr()) };
}

unsafe extern "C" fn mask_create(settings: *mut sys::obs_data_t, context: *mut sys::obs_source_t) -> *mut c_void {
    let mut f = Box::new(SourceMask {
        context,
        effect: std::ptr::null_mut(),
        texrender: std::ptr::null_mut(),
        name: String::new(),
        mode: 0.0,
        invert: false,
        held: None,
    });
    unsafe { mask_update(&mut *f as *mut SourceMask as *mut c_void, settings) };
    Box::into_raw(f) as *mut c_void
}

unsafe extern "C" fn mask_destroy(data: *mut c_void) {
    let mut f = unsafe { Box::from_raw(data as *mut SourceMask) };
    unsafe {
        f.release();
        sys::obs_enter_graphics();
        destroy_effect(&mut f.effect);
        if !f.texrender.is_null() {
            sys::gs_texrender_destroy(f.texrender);
        }
        sys::obs_leave_graphics();
    }
}

unsafe extern "C" fn mask_render(data: *mut c_void, _: *mut sys::gs_effect_t) {
    let f = unsafe { &mut *(data as *mut SourceMask) };
    let (w, h) = unsafe { target_size(f.context) };
    let mask = unsafe { f.mask_source() };
    if w == 0 || h == 0 || mask.is_null() {
        unsafe { sys::obs_source_skip_video_filter(f.context) };
        return;
    }
    unsafe {
        if f.effect.is_null() {
            f.effect = make_effect(&format!("{VERTEX}{MASK_EFFECT}"), c"lightair-object-mask.effect");
        }
        if f.texrender.is_null() {
            f.texrender = sys::gs_texrender_create(sys::GS_RGBA, sys::GS_ZS_NONE);
        }
        if f.effect.is_null() || f.texrender.is_null() {
            sys::obs_source_skip_video_filter(f.context);
            return;
        }
        // the mask's picture, stretched to this source's size
        let (mw, mh) = (sys::obs_source_get_width(mask), sys::obs_source_get_height(mask));
        sys::gs_texrender_reset(f.texrender);
        if sys::gs_texrender_begin(f.texrender, w, h) {
            let mut clear = sys::vec4 { __bindgen_anon_1: sys::vec4__bindgen_ty_1 { ptr: [0.0; 4] } };
            sys::gs_clear(sys::GS_CLEAR_COLOR, &mut clear, 0.0, 0);
            sys::gs_ortho(0.0, w as f32, 0.0, h as f32, -100.0, 100.0);
            if mw > 0 && mh > 0 {
                sys::gs_matrix_push();
                sys::gs_matrix_scale3f(w as f32 / mw as f32, h as f32 / mh as f32, 1.0);
                sys::gs_blend_state_push();
                sys::gs_blend_function(sys::GS_BLEND_ONE, sys::GS_BLEND_INVSRCALPHA);
                sys::obs_source_video_render(mask);
                sys::gs_blend_state_pop();
                sys::gs_matrix_pop();
            }
            sys::gs_texrender_end(f.texrender);
        }
        let tex = sys::gs_texrender_get_texture(f.texrender);
        if !sys::obs_source_process_filter_begin(f.context, sys::GS_RGBA, sys::OBS_ALLOW_DIRECT_RENDERING) {
            return;
        }
        sys::gs_effect_set_texture(param(f.effect, c"mask"), tex);
        sys::gs_effect_set_float(param(f.effect, c"mode"), f.mode);
        sys::gs_effect_set_float(param(f.effect, c"invert"), if f.invert { 1.0 } else { 0.0 });
        sys::obs_source_process_filter_end(f.context, f.effect, w, h);
    }
}

// ---------------------------------------------------------------------------------------------
// Freeze

struct Freeze {
    context: *mut sys::obs_source_t,
    texrender: *mut sys::gs_texrender_t,
    effect: *mut sys::gs_effect_t,
    frozen: bool,
    /// When it was frozen (a new picture is taken at the next frame).
    since: Option<Instant>,
    captured: bool,
    hold_ms: u64,
    fade: f32,
}

unsafe extern "C" fn freeze_name(_: *mut c_void) -> *const c_char {
    c"Freeze".as_ptr()
}

unsafe extern "C" fn freeze_defaults(settings: *mut sys::obs_data_t) {
    unsafe { sys::obs_data_set_default_bool(settings, c"frozen".as_ptr(), true) };
}

unsafe extern "C" fn freeze_update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    let f = unsafe { &mut *(data as *mut Freeze) };
    let frozen = unsafe { sys::obs_data_get_bool(settings, c"frozen".as_ptr()) };
    if frozen && !f.frozen {
        f.since = Some(Instant::now());
        f.captured = false;
    }
    f.frozen = frozen;
    f.hold_ms = unsafe { sys::obs_data_get_int(settings, c"hold_ms".as_ptr()) }.max(0) as u64;
    f.fade = (unsafe { sys::obs_data_get_double(settings, c"fade".as_ptr()) } as f32).clamp(0.0, 1.0);
}

unsafe extern "C" fn freeze_create(settings: *mut sys::obs_data_t, context: *mut sys::obs_source_t) -> *mut c_void {
    let mut f = Box::new(Freeze {
        context,
        texrender: std::ptr::null_mut(),
        effect: std::ptr::null_mut(),
        frozen: false,
        since: None,
        captured: false,
        hold_ms: 0,
        fade: 0.0,
    });
    unsafe { freeze_update(&mut *f as *mut Freeze as *mut c_void, settings) };
    Box::into_raw(f) as *mut c_void
}

unsafe extern "C" fn freeze_destroy(data: *mut c_void) {
    let mut f = unsafe { Box::from_raw(data as *mut Freeze) };
    unsafe {
        sys::obs_enter_graphics();
        if !f.texrender.is_null() {
            sys::gs_texrender_destroy(f.texrender);
        }
        destroy_effect(&mut f.effect);
        sys::obs_leave_graphics();
    }
}

/// Re-freezes when the source comes back on air (a frozen camera isn't stuck on an old moment).
unsafe extern "C" fn freeze_activate(data: *mut c_void) {
    let f = unsafe { &mut *(data as *mut Freeze) };
    if f.frozen {
        f.since = Some(Instant::now());
        f.captured = false;
    }
}

const FADE_EFFECT: &str = r#"
uniform texture2d image;
uniform float opacity;
sampler_state def { Filter = Linear; AddressU = Clamp; AddressV = Clamp; };
float4 PSFade(VertInOut vert_in) : TARGET
{
    float4 c = image.Sample(def, vert_in.uv);
    return float4(c.rgb, c.a * opacity);
}
technique Draw { pass { vertex_shader = VSDefault(vert_in); pixel_shader = PSFade(vert_in); } }
"#;

unsafe extern "C" fn freeze_render(data: *mut c_void, _: *mut sys::gs_effect_t) {
    let f = unsafe { &mut *(data as *mut Freeze) };
    let (w, h) = unsafe { target_size(f.context) };
    let thawed = f.since.is_some_and(|t| f.hold_ms > 0 && t.elapsed().as_millis() as u64 >= f.hold_ms);
    if !f.frozen || thawed || w == 0 || h == 0 {
        unsafe { sys::obs_source_skip_video_filter(f.context) };
        return;
    }
    unsafe {
        if f.texrender.is_null() {
            f.texrender = sys::gs_texrender_create(sys::GS_RGBA, sys::GS_ZS_NONE);
            f.captured = false;
        }
        if f.effect.is_null() {
            f.effect = make_effect(&format!("{VERTEX}{FADE_EFFECT}"), c"lightair-freeze.effect");
        }
        if f.texrender.is_null() || f.effect.is_null() {
            sys::obs_source_skip_video_filter(f.context);
            return;
        }
        let default = sys::obs_get_base_effect(sys::OBS_EFFECT_DEFAULT);
        if !f.captured {
            // the picture at this moment, kept
            sys::gs_texrender_reset(f.texrender);
            if sys::gs_texrender_begin(f.texrender, w, h) {
                let mut clear = sys::vec4 { __bindgen_anon_1: sys::vec4__bindgen_ty_1 { ptr: [0.0; 4] } };
                sys::gs_clear(sys::GS_CLEAR_COLOR, &mut clear, 0.0, 0);
                sys::gs_ortho(0.0, w as f32, 0.0, h as f32, -100.0, 100.0);
                if sys::obs_source_process_filter_begin(f.context, sys::GS_RGBA, sys::OBS_NO_DIRECT_RENDERING) {
                    sys::gs_blend_state_push();
                    sys::gs_blend_function(sys::GS_BLEND_ONE, sys::GS_BLEND_ZERO);
                    sys::obs_source_process_filter_end(f.context, default, w, h);
                    sys::gs_blend_state_pop();
                }
                sys::gs_texrender_end(f.texrender);
                f.captured = true;
            }
        } else if f.fade > 0.0
            && sys::obs_source_process_filter_begin(f.context, sys::GS_RGBA, sys::OBS_ALLOW_DIRECT_RENDERING)
        {
            // some of the live picture shows through: it goes under the frozen one
            sys::obs_source_process_filter_end(f.context, default, w, h);
        }
        let tex = sys::gs_texrender_get_texture(f.texrender);
        if tex.is_null() {
            return;
        }
        sys::gs_effect_set_texture(param(f.effect, c"image"), tex);
        sys::gs_effect_set_float(param(f.effect, c"opacity"), 1.0 - f.fade);
        sys::gs_blend_state_push();
        sys::gs_enable_blending(true);
        sys::gs_blend_function(sys::GS_BLEND_SRCALPHA, sys::GS_BLEND_INVSRCALPHA);
        while sys::gs_effect_loop(f.effect, c"Draw".as_ptr()) {
            sys::gs_draw_sprite(tex, 0, w, h);
        }
        sys::gs_blend_state_pop();
    }
}

// ---------------------------------------------------------------------------------------------
// Custom shader

/// What a Shaderfilter file expects around it (Shaderfilter's documented conventions).
const SHADERFILTER_HEAD: &str = r#"
uniform float4x4 ViewProj;
uniform texture2d image;
uniform float2 uv_offset;
uniform float2 uv_scale;
uniform float2 uv_pixel_interval;
uniform float2 uv_size;
uniform float rand_f;
uniform float rand_instance_f;
uniform float rand_activation_f;
uniform float elapsed_time;
uniform float local_time;
uniform int loops;
uniform float lightair_amount;
"#;

const SHADERFILTER_SAMPLER: &str = r#"
sampler_state textureSampler { Filter = Linear; AddressU = Border; AddressV = Border; BorderColor = 00000000; };
"#;

const SHADERFILTER_VERT: &str = r#"
struct VertData { float4 pos : POSITION; float2 uv : TEXCOORD0; };
VertData mainTransform(VertData v_in)
{
    VertData vert_out;
    vert_out.pos = mul(float4(v_in.pos.xyz, 1.0), ViewProj);
    vert_out.uv = v_in.uv * uv_scale + uv_offset;
    return vert_out;
}
"#;

const SHADERFILTER_TAIL: &str = r#"
float4 lightair_main(VertData v_in) : TARGET
{
    float4 shaded = mainImage(v_in);
    if (lightair_amount >= 0.999) return shaded;
    return lerp(image.Sample(textureSampler, v_in.uv), shaded, lightair_amount);
}
technique Draw { pass { vertex_shader = mainTransform(v_in); pixel_shader = lightair_main(v_in); } }
"#;

/// The effect source for a shader file: as it is when it has its own technique, else wrapped as
/// Shaderfilter wraps its shaders (only what the file doesn't declare itself is added).
pub fn shader_source(file: &str) -> String {
    if file.contains("technique") {
        return file.to_string();
    }
    let mut out = String::new();
    for line in SHADERFILTER_HEAD.lines() {
        // a uniform the file declares itself isn't declared twice
        let name = line.trim().trim_end_matches(';').rsplit(' ').next().unwrap_or("");
        if !name.is_empty() && line.starts_with("uniform") && file.contains(&format!(" {name};")) {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !file.contains("sampler_state textureSampler") {
        out.push_str(SHADERFILTER_SAMPLER);
    }
    let has_struct = file.contains("struct VertData");
    let has_transform = file.contains("mainTransform");
    match (has_struct, has_transform) {
        (false, false) => out.push_str(SHADERFILTER_VERT),
        (false, true) => out.push_str(&SHADERFILTER_VERT[..SHADERFILTER_VERT.find("VertData mainTransform").unwrap_or(0)]),
        (true, false) => out.push_str(&SHADERFILTER_VERT[SHADERFILTER_VERT.find("VertData mainTransform").unwrap_or(0)..]),
        (true, true) => {}
    }
    out.push_str(file);
    out.push_str(SHADERFILTER_TAIL);
    out
}

struct Shader {
    context: *mut sys::obs_source_t,
    effect: *mut sys::gs_effect_t,
    path: String,
    /// The file's text as last loaded; a new one is compiled at the next frame.
    code: Option<String>,
    dirty: bool,
    amount: f32,
    start: Instant,
    rand: f32,
}

unsafe extern "C" fn shader_name(_: *mut c_void) -> *const c_char {
    c"Custom shader".as_ptr()
}

unsafe extern "C" fn shader_update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    let f = unsafe { &mut *(data as *mut Shader) };
    let path = unsafe { get_str(settings, c"path") };
    let code = std::fs::read_to_string(&path).ok();
    if path != f.path || code != f.code {
        f.path = path;
        f.code = code;
        f.dirty = true;
    }
    f.amount = (unsafe { sys::obs_data_get_double(settings, c"amount".as_ptr()) } as f32).clamp(0.0, 1.0);
}

unsafe extern "C" fn shader_defaults(settings: *mut sys::obs_data_t) {
    unsafe { sys::obs_data_set_default_double(settings, c"amount".as_ptr(), 1.0) };
}

unsafe extern "C" fn shader_create(settings: *mut sys::obs_data_t, context: *mut sys::obs_source_t) -> *mut c_void {
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    let mut f = Box::new(Shader {
        context,
        effect: std::ptr::null_mut(),
        path: String::new(),
        code: None,
        dirty: true,
        amount: 1.0,
        start: Instant::now(),
        rand: (seed % 10_000) as f32 / 10_000.0,
    });
    unsafe { shader_update(&mut *f as *mut Shader as *mut c_void, settings) };
    Box::into_raw(f) as *mut c_void
}

unsafe extern "C" fn shader_destroy(data: *mut c_void) {
    let mut f = unsafe { Box::from_raw(data as *mut Shader) };
    unsafe {
        sys::obs_enter_graphics();
        destroy_effect(&mut f.effect);
        sys::obs_leave_graphics();
    }
}

unsafe extern "C" fn shader_render(data: *mut c_void, _: *mut sys::gs_effect_t) {
    let f = unsafe { &mut *(data as *mut Shader) };
    let (w, h) = unsafe { target_size(f.context) };
    unsafe {
        if f.dirty {
            destroy_effect(&mut f.effect);
            match &f.code {
                Some(code) if code.contains("technique") || code.contains("mainImage") => {
                    f.effect = make_effect(&shader_source(code), c"lightair-custom-shader.effect");
                }
                Some(_) => tracing::warn!(path = %f.path, "a shader needs a technique or a mainImage function"),
                None => {}
            }
            f.dirty = false;
        }
        if f.effect.is_null() || w == 0 || h == 0 {
            sys::obs_source_skip_video_filter(f.context);
            return;
        }
        if !sys::obs_source_process_filter_begin(f.context, sys::GS_RGBA, sys::OBS_ALLOW_DIRECT_RENDERING) {
            return;
        }
        let e = f.effect;
        let set2 = |n: &CStr, x: f32, y: f32| {
            let p = param(e, n);
            if !p.is_null() {
                let v = vec2(x, y);
                sys::gs_effect_set_vec2(p, &v);
            }
        };
        let set1 = |n: &CStr, x: f32| {
            let p = param(e, n);
            if !p.is_null() {
                sys::gs_effect_set_float(p, x);
            }
        };
        let t = f.start.elapsed().as_secs_f32();
        set2(c"uv_size", w as f32, h as f32);
        set2(c"uv_pixel_interval", 1.0 / w as f32, 1.0 / h as f32);
        set2(c"uv_scale", 1.0, 1.0);
        set2(c"uv_offset", 0.0, 0.0);
        set1(c"elapsed_time", t);
        set1(c"local_time", t);
        set1(c"rand_f", rand_now());
        set1(c"rand_instance_f", f.rand);
        set1(c"rand_activation_f", f.rand);
        set1(c"lightair_amount", f.amount);
        sys::obs_source_process_filter_end(f.context, e, w, h);
    }
}

fn rand_now() -> f32 {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    (n.wrapping_mul(2_654_435_761) % 100_000) as f32 / 100_000.0
}

/// Registers the filters with libobs (once; after startup).
pub fn register() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = c"lightair_source_mask".as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_FILTER;
        info.output_flags = sys::OBS_SOURCE_VIDEO;
        info.get_name = Some(mask_name);
        info.create = Some(mask_create);
        info.destroy = Some(mask_destroy);
        info.update = Some(mask_update);
        info.video_render = Some(mask_render);
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());

        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = c"lightair_freeze".as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_FILTER;
        info.output_flags = sys::OBS_SOURCE_VIDEO;
        info.get_name = Some(freeze_name);
        info.create = Some(freeze_create);
        info.destroy = Some(freeze_destroy);
        info.update = Some(freeze_update);
        info.get_defaults = Some(freeze_defaults);
        info.activate = Some(freeze_activate);
        info.video_render = Some(freeze_render);
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());

        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = c"lightair_shader".as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_FILTER;
        info.output_flags = sys::OBS_SOURCE_VIDEO;
        info.get_name = Some(shader_name);
        info.create = Some(shader_create);
        info.destroy = Some(shader_destroy);
        info.update = Some(shader_update);
        info.get_defaults = Some(shader_defaults);
        info.video_render = Some(shader_render);
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}
