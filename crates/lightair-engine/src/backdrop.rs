//! Backdrops (docs/plans/screen-studio.md, "Look and framing"): a gradient behind a screen
//! recording, with a soft rounded shadow where the screen sits, drawn by one shader at the
//! canvas's size. Lightair's own gradients; nothing copied.
//!
//! Settings: `width`, `height`; `preset` (an index into [`PRESETS`], or -1 for `color_a`..`color_c`
//! as 0xAABBGGRR); `angle` (degrees); `shadow` (bool) with `shadow_x`, `shadow_y`, `shadow_w`,
//! `shadow_h` (the screen's box, canvas pixels), `shadow_radius`, `shadow_blur` and
//! `shadow_opacity`.

use std::ffi::{CString, c_char, c_void};

use crate::sys;

pub const KIND: &str = "lightair_backdrop";

/// (name, three colours as 0xRRGGBB, angle in degrees).
pub const PRESETS: &[(&str, [u32; 3], f32)] = &[
    ("Dusk", [0x1E1B4B, 0x6D28D9, 0xF472B6], 35.0),
    ("Ocean", [0x0B3B5C, 0x1D7FA8, 0x7DD3C0], 120.0),
    ("Ember", [0x3B0A0A, 0xB4361C, 0xF6A04D], 60.0),
    ("Mint", [0x0F3D3A, 0x2F9E7A, 0xC8F2D8], 150.0),
    ("Graphite", [0x111214, 0x2A2D33, 0x4A4F59], 90.0),
    ("Sand", [0x8A6A4A, 0xD8BC94, 0xF3E6D0], 20.0),
];

const EFFECT: &str = r#"
uniform float4x4 ViewProj;
uniform float2 size;
uniform float4 color_a;
uniform float4 color_b;
uniform float4 color_c;
uniform float2 dir;
uniform float4 shadow_rect;
uniform float shadow_radius;
uniform float shadow_blur;
uniform float shadow_opacity;
struct VertInOut { float4 pos : POSITION; float2 uv : TEXCOORD0; };
VertInOut VSDefault(VertInOut vert_in)
{
    VertInOut vert_out;
    vert_out.pos = mul(float4(vert_in.pos.xyz, 1.0), ViewProj);
    vert_out.uv = vert_in.uv;
    return vert_out;
}
float4 PSDraw(VertInOut vert_in) : TARGET
{
    float2 p = vert_in.uv * size;
    float t = saturate(dot(vert_in.uv - float2(0.5, 0.5), dir) + 0.5);
    float3 lo = lerp(color_a.rgb, color_b.rgb, saturate(t * 2.0));
    float3 hi = lerp(color_b.rgb, color_c.rgb, saturate(t * 2.0 - 1.0));
    float3 c = lerp(lo, hi, step(0.5, t));
    float2 center = shadow_rect.xy + shadow_rect.zw * 0.5 + float2(0.0, shadow_blur * 0.35);
    float2 q = abs(p - center) - shadow_rect.zw * 0.5 + float2(shadow_radius, shadow_radius);
    float d = length(max(q, float2(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - shadow_radius;
    float s = 1.0 - smoothstep(-shadow_blur, shadow_blur, d);
    c = c * (1.0 - shadow_opacity * s);
    return float4(c, 1.0);
}
technique Draw { pass { vertex_shader = VSDefault(vert_in); pixel_shader = PSDraw(vert_in); } }
"#;

struct Params {
    width: u32,
    height: u32,
    colors: [[f32; 3]; 3],
    angle: f32,
    shadow: Option<([f32; 4], f32, f32, f32)>,
}

struct Backdrop {
    p: Params,
    effect: *mut sys::gs_effect_t,
}

fn rgb(c: u32) -> [f32; 3] {
    // sRGB colours, as the canvas draws them
    [((c >> 16) & 0xFF) as f32 / 255.0, ((c >> 8) & 0xFF) as f32 / 255.0, (c & 0xFF) as f32 / 255.0]
}

/// 0xAABBGGRR (OBS colours) as 0xRRGGBB.
fn from_obs(c: u32) -> u32 {
    ((c & 0xFF) << 16) | (c & 0xFF00) | ((c >> 16) & 0xFF)
}

unsafe fn read(settings: *mut sys::obs_data_t) -> Params {
    let int = |k: &std::ffi::CStr| unsafe { sys::obs_data_get_int(settings, k.as_ptr()) };
    let float = |k: &std::ffi::CStr| unsafe { sys::obs_data_get_double(settings, k.as_ptr()) } as f32;
    let preset = int(c"preset");
    let (colors, angle) = match PRESETS.get(preset.max(0) as usize).filter(|_| preset >= 0) {
        Some((_, c, a)) => ([rgb(c[0]), rgb(c[1]), rgb(c[2])], *a),
        None => (
            [rgb(from_obs(int(c"color_a") as u32)), rgb(from_obs(int(c"color_b") as u32)), rgb(from_obs(int(c"color_c") as u32))],
            float(c"angle"),
        ),
    };
    let shadow = unsafe { sys::obs_data_get_bool(settings, c"shadow".as_ptr()) }.then(|| {
        (
            [float(c"shadow_x"), float(c"shadow_y"), float(c"shadow_w"), float(c"shadow_h")],
            float(c"shadow_radius"),
            float(c"shadow_blur").max(1.0),
            float(c"shadow_opacity").clamp(0.0, 1.0),
        )
    });
    Params { width: int(c"width").max(2) as u32, height: int(c"height").max(2) as u32, colors, angle, shadow }
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"Backdrop".as_ptr()
}

unsafe extern "C" fn get_defaults(settings: *mut sys::obs_data_t) {
    unsafe {
        sys::obs_data_set_default_int(settings, c"width".as_ptr(), 1920);
        sys::obs_data_set_default_int(settings, c"height".as_ptr(), 1080);
        sys::obs_data_set_default_double(settings, c"shadow_radius".as_ptr(), 24.0);
        sys::obs_data_set_default_double(settings, c"shadow_blur".as_ptr(), 40.0);
        sys::obs_data_set_default_double(settings, c"shadow_opacity".as_ptr(), 0.45);
    }
}

unsafe extern "C" fn create(settings: *mut sys::obs_data_t, _: *mut sys::obs_source_t) -> *mut c_void {
    Box::into_raw(Box::new(Backdrop { p: unsafe { read(settings) }, effect: std::ptr::null_mut() })) as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let b = unsafe { Box::from_raw(data as *mut Backdrop) };
    if !b.effect.is_null() {
        unsafe {
            sys::obs_enter_graphics();
            sys::gs_effect_destroy(b.effect);
            sys::obs_leave_graphics();
        }
    }
}

unsafe extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    let b = unsafe { &mut *(data as *mut Backdrop) };
    b.p = unsafe { read(settings) };
}

unsafe extern "C" fn width(data: *mut c_void) -> u32 {
    unsafe { &*(data as *const Backdrop) }.p.width
}

unsafe extern "C" fn height(data: *mut c_void) -> u32 {
    unsafe { &*(data as *const Backdrop) }.p.height
}

unsafe extern "C" fn render(data: *mut c_void, _: *mut sys::gs_effect_t) {
    let b = unsafe { &mut *(data as *mut Backdrop) };
    unsafe {
        if b.effect.is_null() {
            let code = CString::new(EFFECT).unwrap_or_default();
            let mut err: *mut c_char = std::ptr::null_mut();
            b.effect = sys::gs_effect_create(code.as_ptr(), c"lightair-backdrop.effect".as_ptr(), &mut err);
            if !err.is_null() {
                tracing::warn!(error = %std::ffi::CStr::from_ptr(err).to_string_lossy(), "backdrop effect");
                sys::bfree(err as *mut c_void);
            }
            if b.effect.is_null() {
                return;
            }
        }
        let e = b.effect;
        let param = |n: &std::ffi::CStr| sys::gs_effect_get_param_by_name(e, n.as_ptr());
        let p = &b.p;
        let size = sys::vec2 { __bindgen_anon_1: sys::vec2__bindgen_ty_1 { ptr: [p.width as f32, p.height as f32] } };
        sys::gs_effect_set_vec2(param(c"size"), &size);
        for (name, c) in [(c"color_a", p.colors[0]), (c"color_b", p.colors[1]), (c"color_c", p.colors[2])] {
            let v = sys::vec4 { __bindgen_anon_1: sys::vec4__bindgen_ty_1 { ptr: [c[0], c[1], c[2], 1.0] } };
            sys::gs_effect_set_vec4(param(name), &v);
        }
        let a = p.angle.to_radians();
        let dir = sys::vec2 { __bindgen_anon_1: sys::vec2__bindgen_ty_1 { ptr: [a.cos(), a.sin()] } };
        sys::gs_effect_set_vec2(param(c"dir"), &dir);
        let (rect, radius, blur, opacity) = p.shadow.unwrap_or(([0.0; 4], 0.0, 1.0, 0.0));
        let r = sys::vec4 { __bindgen_anon_1: sys::vec4__bindgen_ty_1 { ptr: rect } };
        sys::gs_effect_set_vec4(param(c"shadow_rect"), &r);
        sys::gs_effect_set_float(param(c"shadow_radius"), radius);
        sys::gs_effect_set_float(param(c"shadow_blur"), blur);
        sys::gs_effect_set_float(param(c"shadow_opacity"), opacity);
        while sys::gs_effect_loop(e, c"Draw".as_ptr()) {
            sys::gs_draw_sprite(std::ptr::null_mut(), 0, p.width, p.height);
        }
    }
}

/// Registers the source with libobs (once; after startup).
pub fn register() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = c"lightair_backdrop".as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_INPUT;
        // it draws with its own effect
        info.output_flags = sys::OBS_SOURCE_VIDEO | sys::OBS_SOURCE_CUSTOM_DRAW;
        info.get_name = Some(get_name);
        info.create = Some(create);
        info.destroy = Some(destroy);
        info.update = Some(update);
        info.get_defaults = Some(get_defaults);
        info.get_width = Some(width);
        info.get_height = Some(height);
        info.video_render = Some(render);
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}
