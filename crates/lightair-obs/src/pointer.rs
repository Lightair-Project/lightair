//! The drawn pointer's libobs filter: draws `lightair_host::pointer`'s state (the pointer, its
//! clicks and its follow-zoom) on a screen capture with libobs's graphics.

use std::ffi::{CString, c_char, c_void};

use lightair_render::shapes::{Bitmap, RES, arrow, crosshair, disc, dot, ibeam, ring};

pub use lightair_host::pointer::*;

use lightair_platform::{input, system_pointer};

use crate::sys;

// ------------------------------------------------------------------ the filter

const EFFECT: &str = r#"
uniform float4x4 ViewProj;
uniform texture2d image;
uniform float opacity;
sampler_state def_sampler { Filter = Linear; AddressU = Clamp; AddressV = Clamp; };
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
    return image.Sample(def_sampler, vert_in.uv) * opacity;
}
technique Draw { pass { vertex_shader = VSDefault(vert_in); pixel_shader = PSDraw(vert_in); } }
"#;

/// The capture drawn zoomed: `view` is the part shown (x, y, w, h as fractions of it).
const ZOOM_EFFECT: &str = r#"
uniform float4x4 ViewProj;
uniform texture2d image;
uniform float4 view;
sampler_state def_sampler { Filter = Linear; AddressU = Clamp; AddressV = Clamp; };
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
    return image.Sample(def_sampler, view.xy + vert_in.uv * view.zw);
}
technique Draw { pass { vertex_shader = VSDefault(vert_in); pixel_shader = PSDraw(vert_in); } }
"#;

/// The textures, made on the graphics thread when first drawn (and again when the colour changes).
struct Textures {
    effect: *mut sys::gs_effect_t,
    image: *mut sys::gs_eparam_t,
    opacity: *mut sys::gs_eparam_t,
    zoom: *mut sys::gs_effect_t,
    view: *mut sys::gs_eparam_t,
    arrow: (*mut sys::gs_texture_t, (f32, f32)),
    ibeam: (*mut sys::gs_texture_t, (f32, f32)),
    cross: (*mut sys::gs_texture_t, (f32, f32)),
    dot: *mut sys::gs_texture_t,
    ring: *mut sys::gs_texture_t,
    click_ring: *mut sys::gs_texture_t,
    click_disc: *mut sys::gs_texture_t,
    color: u32,
}

struct Filter {
    context: *mut sys::obs_source_t,
    pointer: Pointer,
    textures: Option<Textures>,
    /// The system pointer's picture as a texture, and the picture's `serial` it was made from.
    picture_tex: Option<(*mut sys::gs_texture_t, u64)>,
}

/// What a screen capture shows, in global display points: its display (by the display it's
/// set to), or its window (where it is now). `None`: the main display.
fn capture_frame(context: *mut sys::obs_source_t) -> Option<(f32, f32, f32, f32)> {
    let parent = unsafe { sys::obs_filter_get_parent(context) };
    if parent.is_null() {
        return None;
    }
    let id = unsafe { std::ffi::CStr::from_ptr(sys::obs_source_get_id(parent)) }.to_string_lossy().into_owned();
    let settings = unsafe { sys::obs_source_get_settings(parent) };
    if settings.is_null() {
        return None;
    }
    let text = |k: &std::ffi::CStr| {
        let p = unsafe { sys::obs_data_get_string(settings, k.as_ptr()) };
        if p.is_null() { String::new() } else { unsafe { std::ffi::CStr::from_ptr(p) }.to_string_lossy().into_owned() }
    };
    let int = |k: &std::ffi::CStr| unsafe { sys::obs_data_get_int(settings, k.as_ptr()) };
    let frame = match id.as_str() {
        // macOS: a display (0), a window (1) or an app's windows on its display (2)
        "screen_capture" | "lightair_screen_capture" => match int(c"type") {
            1 => input::window_frame(&int(c"window").to_string()),
            _ => input::display_frame(&text(c"display_uuid")),
        },
        // Windows
        "window_capture" | "game_capture" => input::window_frame(&text(c"window")),
        _ => None,
    };
    unsafe { sys::obs_data_release(settings) };
    frame
}

fn rgb_of(c: u32) -> [f32; 3] {
    [(c & 0xFF) as f32 / 255.0, ((c >> 8) & 0xFF) as f32 / 255.0, ((c >> 16) & 0xFF) as f32 / 255.0]
}

unsafe fn texture(b: &Bitmap) -> *mut sys::gs_texture_t {
    let mut data = b.rgba.as_ptr();
    unsafe { sys::gs_texture_create(RES as u32, RES as u32, sys::GS_RGBA, 1, &mut data, 0) }
}

unsafe fn make_textures(color: u32) -> Option<Textures> {
    let code = CString::new(EFFECT).ok()?;
    let name = c"lightair-cursor.effect";
    let mut err: *mut c_char = std::ptr::null_mut();
    unsafe {
        let effect = sys::gs_effect_create(code.as_ptr(), name.as_ptr(), &mut err);
        if !err.is_null() {
            tracing::warn!(error = %std::ffi::CStr::from_ptr(err).to_string_lossy(), "pointer effect");
            sys::bfree(err as *mut c_void);
        }
        if effect.is_null() {
            return None;
        }
        let zcode = CString::new(ZOOM_EFFECT).ok()?;
        let mut zerr: *mut c_char = std::ptr::null_mut();
        let zoom = sys::gs_effect_create(zcode.as_ptr(), c"lightair-zoom.effect".as_ptr(), &mut zerr);
        if !zerr.is_null() {
            tracing::warn!(error = %std::ffi::CStr::from_ptr(zerr).to_string_lossy(), "zoom effect");
            sys::bfree(zerr as *mut c_void);
        }
        let (a, i, x) = (arrow(), ibeam(), crosshair());
        let rgb = rgb_of(color);
        Some(Textures {
            effect,
            image: sys::gs_effect_get_param_by_name(effect, c"image".as_ptr()),
            opacity: sys::gs_effect_get_param_by_name(effect, c"opacity".as_ptr()),
            view: if zoom.is_null() {
                std::ptr::null_mut()
            } else {
                sys::gs_effect_get_param_by_name(zoom, c"view".as_ptr())
            },
            zoom,
            arrow: (texture(&a), a.hot),
            ibeam: (texture(&i), i.hot),
            cross: (texture(&x), x.hot),
            dot: texture(&dot(rgb)),
            ring: texture(&ring(rgb, 0.36, 0.08)),
            click_ring: texture(&ring([1.0, 1.0, 1.0], 0.44, 0.05)),
            click_disc: texture(&disc(rgb)),
            color,
        })
    }
}

unsafe fn free_textures(t: Textures) {
    unsafe {
        for tex in [t.arrow.0, t.ibeam.0, t.cross.0, t.dot, t.ring, t.click_ring, t.click_disc] {
            if !tex.is_null() {
                sys::gs_texture_destroy(tex);
            }
        }
        sys::gs_effect_destroy(t.effect);
        if !t.zoom.is_null() {
            sys::gs_effect_destroy(t.zoom);
        }
    }
}

unsafe fn read_look(settings: *mut sys::obs_data_t) -> Style {
    // the settings with the filter's defaults (`get_defaults`, the same as `Style::from_json`'s)
    let json = unsafe { sys::obs_data_get_json_with_defaults(settings) };
    let value = if json.is_null() {
        serde_json::Value::Null
    } else {
        serde_json::from_str(&unsafe { std::ffi::CStr::from_ptr(json) }.to_string_lossy()).unwrap_or_default()
    };
    Style::from_json(&value)
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"Drawn pointer".as_ptr()
}

unsafe extern "C" fn get_defaults(settings: *mut sys::obs_data_t) {
    unsafe {
        sys::obs_data_set_default_double(settings, c"size".as_ptr(), 1.5);
        sys::obs_data_set_default_double(settings, c"smoothing".as_ptr(), 0.5);
        sys::obs_data_set_default_bool(settings, c"tilt".as_ptr(), true);
        sys::obs_data_set_default_double(settings, c"idle_s".as_ptr(), 3.0);
        sys::obs_data_set_default_int(settings, c"clicks".as_ptr(), 2);
        sys::obs_data_set_default_int(settings, c"color".as_ptr(), 0xFFFF_9000);
        sys::obs_data_set_default_bool(settings, c"draw".as_ptr(), true);
        sys::obs_data_set_default_double(settings, c"zoom_scale".as_ptr(), 2.0);
        sys::obs_data_set_default_double(settings, c"zoom_hold_s".as_ptr(), 2.0);
        sys::obs_data_set_default_double(settings, c"zoom_motion".as_ptr(), 0.5);
        sys::obs_data_set_default_bool(settings, c"zoom_typing".as_ptr(), true);
        sys::obs_data_set_default_bool(settings, c"zoom_badge".as_ptr(), true);
    }
}

unsafe extern "C" fn create(settings: *mut sys::obs_data_t, context: *mut sys::obs_source_t) -> *mut c_void {
    let f =
        Filter { context, pointer: Pointer::new(unsafe { read_look(settings) }), textures: None, picture_tex: None };
    Box::into_raw(Box::new(f)) as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let f = unsafe { Box::from_raw(data as *mut Filter) };
    report_zoom(f.context as usize, None);
    if f.textures.is_some() || f.picture_tex.is_some() {
        unsafe {
            sys::obs_enter_graphics();
            if let Some(t) = f.textures {
                free_textures(t);
            }
            if let Some((tex, _)) = f.picture_tex {
                sys::gs_texture_destroy(tex);
            }
            sys::obs_leave_graphics();
        }
    }
}

unsafe extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    let f = unsafe { &mut *(data as *mut Filter) };
    f.pointer.set_look(unsafe { read_look(settings) });
}

unsafe extern "C" fn tick(data: *mut c_void, seconds: f32) {
    let f = unsafe { &mut *(data as *mut Filter) };
    let target = unsafe { sys::obs_filter_get_target(f.context) };
    let size = if target.is_null() {
        (0, 0)
    } else {
        unsafe { (sys::obs_source_get_base_width(target), sys::obs_source_get_base_height(target)) }
    };
    let context = f.context;
    f.pointer.tick(seconds, size, || capture_frame(context));
    // what viewers see, for the badge: zoomed in, on a capture that's live
    let parent = unsafe { sys::obs_filter_get_parent(f.context) };
    let live = !parent.is_null() && unsafe { sys::obs_source_active(parent) };
    let zoom = f.pointer.zoom().filter(|_| live && f.pointer.look().badge());
    report_zoom(f.context as usize, zoom);
}

/// Draws `tex` with its hot spot at `at`, `size` pixels square, turned by `angle` degrees.
unsafe fn draw(
    t: &Textures,
    tex: *mut sys::gs_texture_t,
    at: (f32, f32),
    size: f32,
    hot: (f32, f32),
    angle: f32,
    opacity: f32,
) {
    if tex.is_null() || opacity <= 0.001 {
        return;
    }
    unsafe {
        sys::gs_effect_set_texture(t.image, tex);
        sys::gs_effect_set_float(t.opacity, opacity);
        sys::gs_matrix_push();
        sys::gs_matrix_translate3f(at.0, at.1, 0.0);
        sys::gs_matrix_rotaa4f(0.0, 0.0, 1.0, angle.to_radians());
        sys::gs_matrix_translate3f(-hot.0 * size, -hot.1 * size, 0.0);
        sys::gs_draw_sprite(tex, 0, size as u32, size as u32);
        sys::gs_matrix_pop();
    }
}

unsafe extern "C" fn render(data: *mut c_void, _: *mut sys::gs_effect_t) {
    let f = unsafe { &mut *(data as *mut Filter) };
    let target = unsafe { sys::obs_filter_get_target(f.context) };
    let (tw, th) = if target.is_null() {
        (0, 0)
    } else {
        unsafe { (sys::obs_source_get_base_width(target), sys::obs_source_get_base_height(target)) }
    };
    let color = f.pointer.look().color();
    if f.textures.as_ref().is_some_and(|t| t.color != color)
        && let Some(old) = f.textures.take()
    {
        unsafe { free_textures(old) };
    }
    if f.textures.is_none() {
        f.textures = unsafe { make_textures(color) };
    }
    let zoomable = f.textures.as_ref().is_some_and(|t| !t.zoom.is_null());
    let drawing =
        if zoomable { f.pointer.drawing((tw, th)) } else { Drawing { view: None, ..f.pointer.drawing((tw, th)) } };
    // the capture: zoomed, or as it is
    match (drawing.view, f.textures.as_ref()) {
        (Some((vx, vy, vw, vh)), Some(t)) => unsafe {
            if sys::obs_source_process_filter_begin(f.context, sys::GS_RGBA, sys::OBS_ALLOW_DIRECT_RENDERING) {
                let v = sys::vec4 { __bindgen_anon_1: sys::vec4__bindgen_ty_1 { ptr: [vx, vy, vw, vh] } };
                sys::gs_effect_set_vec4(t.view, &v);
                sys::obs_source_process_filter_end(f.context, t.zoom, tw, th);
            }
        },
        _ => unsafe { sys::obs_source_skip_video_filter(f.context) },
    }
    if drawing.sprites.is_empty() {
        return;
    }
    // the system pointer's picture, as a texture when it changes
    if let Some(p) = f.pointer.picture()
        && f.picture_tex.is_none_or(|(_, serial)| serial != p.serial)
    {
        unsafe {
            if let Some((old, _)) = f.picture_tex.take() {
                sys::gs_texture_destroy(old);
            }
            let side = system_pointer::SIDE as u32;
            let mut data = p.rgba.as_ptr();
            let tex = sys::gs_texture_create(side, side, sys::GS_RGBA, 1, &mut data, 0);
            if !tex.is_null() {
                f.picture_tex = Some((tex, p.serial));
            }
        }
    }
    let Some(t) = &f.textures else { return };
    let system =
        f.pointer.picture().zip(f.picture_tex).filter(|(p, (_, serial))| p.serial == *serial).map(|(_, (tex, _))| tex);
    unsafe {
        sys::gs_blend_state_push();
        sys::gs_enable_blending(true);
        sys::gs_blend_function(sys::GS_BLEND_ONE, sys::GS_BLEND_INVSRCALPHA);
        let tech = sys::gs_effect_get_technique(t.effect, c"Draw".as_ptr());
        sys::gs_technique_begin(tech);
        sys::gs_technique_begin_pass(tech, 0);
        for sprite in &drawing.sprites {
            let tex = match sprite.art {
                Art::Arrow => t.arrow.0,
                Art::IBeam => t.ibeam.0,
                Art::Crosshair => t.cross.0,
                Art::Dot => t.dot,
                Art::Ring => t.ring,
                Art::ClickRing => t.click_ring,
                Art::ClickDisc => t.click_disc,
                Art::System => system.unwrap_or(std::ptr::null_mut()),
            };
            draw(t, tex, sprite.at, sprite.size, sprite.hot, sprite.angle, sprite.opacity);
        }
        sys::gs_technique_end_pass(tech);
        sys::gs_technique_end(tech);
        sys::gs_blend_state_pop();
    }
}

/// Registers the filter with libobs (once; after startup).
pub fn register() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = c"lightair_pointer".as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_FILTER;
        info.output_flags = sys::OBS_SOURCE_VIDEO;
        info.get_name = Some(get_name);
        info.create = Some(create);
        info.destroy = Some(destroy);
        info.update = Some(update);
        info.get_defaults = Some(get_defaults);
        info.video_tick = Some(tick);
        info.video_render = Some(render);
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}

