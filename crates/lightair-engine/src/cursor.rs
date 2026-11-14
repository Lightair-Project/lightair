//! The drawn pointer (docs/plans/screen-studio.md, "Cursor"): a filter on a screen capture that
//! draws the pointer itself, so it can be bigger, smoother and styled. The capture's own cursor
//! is turned off while it's on (`reconcile::input_settings`).
//!
//! - The pointer comes from `lightair_platform::input` each frame and is followed by a critically
//!   damped spring (smoothing), tilted by its speed, and faded out when it rests.
//! - Shapes are Lightair's own, drawn here at a high resolution (an arrow, an I-beam, a
//!   crosshair; a dot and a ring as styles), so they stay sharp at any size.
//! - Clicks (when the OS lets Lightair see them) draw a circle or a ripple.
//!
//! Positions map from display points to the capture's pixels through the main display's frame:
//! a capture of another display places the pointer as if it were the main one (a later phase
//! maps each display).

use std::collections::VecDeque;
use std::ffi::{CString, c_char, c_void};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use lightair_platform::input::{self, Event, Recorder, Shape};

use crate::sys;

pub const KIND: &str = lightair_core::catalog::CURSOR_KIND;

/// Texture size of a drawn shape (a 32-point pointer at 4×, enough for size 4 on Retina).
const RES: usize = 128;
/// The pointer's size in display points at size 1.
const POINTS: f32 = 22.0;
/// How long a click's effect lasts.
const CLICK: Duration = Duration::from_millis(450);

// ------------------------------------------------------------------ shapes

/// A shape as premultiplied RGBA, RES×RES, and its hot spot as a fraction of the size.
struct Bitmap {
    rgba: Vec<u8>,
    hot: (f32, f32),
}

fn segment_distance(p: (f32, f32), a: (f32, f32), b: (f32, f32)) -> f32 {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let len = dx * dx + dy * dy;
    let t = if len > 0.0 { (((p.0 - a.0) * dx + (p.1 - a.1) * dy) / len).clamp(0.0, 1.0) } else { 0.0 };
    let (cx, cy) = (a.0 + t * dx, a.1 + t * dy);
    ((p.0 - cx).powi(2) + (p.1 - cy).powi(2)).sqrt()
}

fn inside(p: (f32, f32), poly: &[(f32, f32)]) -> bool {
    let mut c = false;
    let mut j = poly.len() - 1;
    for i in 0..poly.len() {
        let (a, b) = (poly[i], poly[j]);
        if (a.1 > p.1) != (b.1 > p.1) && p.0 < (b.0 - a.0) * (p.1 - a.1) / (b.1 - a.1) + a.0 {
            c = !c;
        }
        j = i;
    }
    c
}

/// Rasterises with 4×4 samples per pixel: `colour(x, y)` (in 0..1 units) gives RGBA or nothing.
fn raster(hot: (f32, f32), colour: impl Fn((f32, f32)) -> Option<[f32; 4]>) -> Bitmap {
    let mut rgba = vec![0u8; RES * RES * 4];
    for y in 0..RES {
        for x in 0..RES {
            let mut acc = [0f32; 4];
            for sy in 0..4 {
                for sx in 0..4 {
                    let p = ((x as f32 + (sx as f32 + 0.5) / 4.0) / RES as f32, (y as f32 + (sy as f32 + 0.5) / 4.0) / RES as f32);
                    if let Some(c) = colour(p) {
                        // premultiplied
                        acc[0] += c[0] * c[3];
                        acc[1] += c[1] * c[3];
                        acc[2] += c[2] * c[3];
                        acc[3] += c[3];
                    }
                }
            }
            let i = (y * RES + x) * 4;
            for k in 0..4 {
                rgba[i + k] = (acc[k] / 16.0 * 255.0).round().clamp(0.0, 255.0) as u8;
            }
        }
    }
    Bitmap { rgba, hot }
}

/// A shape with a white body and a dark edge (each platform's pointers look like this).
fn outlined(poly: &[(f32, f32)], hot: (f32, f32)) -> Bitmap {
    let edge = 0.035;
    raster(hot, |p| {
        let near = (0..poly.len()).map(|i| segment_distance(p, poly[i], poly[(i + 1) % poly.len()])).fold(f32::MAX, f32::min);
        if inside(p, poly) {
            Some(if near < edge { [0.0, 0.0, 0.0, 1.0] } else { [1.0, 1.0, 1.0, 1.0] })
        } else if near < edge * 0.6 {
            Some([0.0, 0.0, 0.0, 1.0])
        } else if near < edge * 2.2 {
            // a soft shadow
            Some([0.0, 0.0, 0.0, 0.18 * (1.0 - near / (edge * 2.2))])
        } else {
            None
        }
    })
}

fn arrow() -> Bitmap {
    let s = 0.86;
    let poly: Vec<(f32, f32)> = [(0.0, 0.0), (0.0, 0.84), (0.22, 0.65), (0.36, 0.98), (0.49, 0.92), (0.35, 0.61), (0.62, 0.61)]
        .iter()
        .map(|(x, y)| (0.08 + x * s, 0.04 + y * s))
        .collect();
    outlined(&poly, (0.08, 0.04))
}

fn ibeam() -> Bitmap {
    // a stem with serifs, as one outline
    let poly = [
        (0.36, 0.08),
        (0.64, 0.08),
        (0.64, 0.15),
        (0.54, 0.15),
        (0.54, 0.85),
        (0.64, 0.85),
        (0.64, 0.92),
        (0.36, 0.92),
        (0.36, 0.85),
        (0.46, 0.85),
        (0.46, 0.15),
        (0.36, 0.15),
    ];
    outlined(&poly, (0.5, 0.5))
}

fn crosshair() -> Bitmap {
    let poly = [
        (0.46, 0.1),
        (0.54, 0.1),
        (0.54, 0.46),
        (0.9, 0.46),
        (0.9, 0.54),
        (0.54, 0.54),
        (0.54, 0.9),
        (0.46, 0.9),
        (0.46, 0.54),
        (0.1, 0.54),
        (0.1, 0.46),
        (0.46, 0.46),
    ];
    outlined(&poly, (0.5, 0.5))
}

/// A filled dot with a white rim.
fn dot(rgb: [f32; 3]) -> Bitmap {
    raster((0.5, 0.5), |(x, y)| {
        let r = ((x - 0.5).powi(2) + (y - 0.5).powi(2)).sqrt();
        if r < 0.24 {
            Some([rgb[0], rgb[1], rgb[2], 1.0])
        } else if r < 0.29 {
            Some([1.0, 1.0, 1.0, 1.0])
        } else if r < 0.36 {
            Some([0.0, 0.0, 0.0, 0.2 * (1.0 - (r - 0.29) / 0.07)])
        } else {
            None
        }
    })
}

/// A ring (the Ring style, and click effects).
fn ring(rgb: [f32; 3], radius: f32, width: f32) -> Bitmap {
    raster((0.5, 0.5), |(x, y)| {
        let r = ((x - 0.5).powi(2) + (y - 0.5).powi(2)).sqrt();
        ((r - radius).abs() < width / 2.0).then_some([rgb[0], rgb[1], rgb[2], 1.0])
    })
}

/// A filled disc (click circles).
fn disc(rgb: [f32; 3]) -> Bitmap {
    raster((0.5, 0.5), |(x, y)| {
        let r = ((x - 0.5).powi(2) + (y - 0.5).powi(2)).sqrt();
        (r < 0.48).then_some([rgb[0], rgb[1], rgb[2], 1.0])
    })
}

// ------------------------------------------------------------------ clicks

/// Clicks seen lately, for every pointer filter (one listener for all of them).
struct Clicks {
    recorder: Option<Recorder>,
    started: Instant,
    recent: VecDeque<(Instant, f32, f32)>,
    users: usize,
}

static CLICKS: Mutex<Option<Clicks>> = Mutex::new(None);

fn clicks_join() {
    let mut c = CLICKS.lock().unwrap();
    let c = c.get_or_insert_with(|| Clicks { recorder: None, started: Instant::now(), recent: VecDeque::new(), users: 0 });
    c.users += 1;
    if c.recorder.is_none() && input::can_listen() {
        c.started = Instant::now();
        c.recorder = Some(Recorder::start(30));
    }
}

fn clicks_leave() {
    let mut guard = CLICKS.lock().unwrap();
    if let Some(c) = guard.as_mut() {
        c.users = c.users.saturating_sub(1);
        if c.users == 0 {
            *guard = None;
        }
    }
}

/// Clicks in the last `CLICK`, oldest first.
fn recent_clicks() -> Vec<(Instant, f32, f32)> {
    let mut guard = CLICKS.lock().unwrap();
    let Some(c) = guard.as_mut() else { return Vec::new() };
    if let Some(r) = &c.recorder {
        for s in r.take() {
            if let Event::Down { x, y, .. } = s.event {
                c.recent.push_back((c.started + s.at, x, y));
            }
        }
    }
    let now = Instant::now();
    while c.recent.front().is_some_and(|(t, ..)| now.duration_since(*t) > CLICK) {
        c.recent.pop_front();
    }
    c.recent.iter().copied().collect()
}

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

#[derive(Clone, Copy, PartialEq)]
struct Look {
    style: i64,
    size: f32,
    smoothing: f32,
    tilt: bool,
    idle: f32,
    clicks: i64,
    arrow_only: bool,
    /// 0xAABBGGRR
    color: u32,
}

/// The textures, made on the graphics thread when first drawn (and again when the colour changes).
struct Textures {
    effect: *mut sys::gs_effect_t,
    image: *mut sys::gs_eparam_t,
    opacity: *mut sys::gs_eparam_t,
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
    look: Look,
    /// Followed position (display points), its velocity, the tilt.
    pos: Option<(f32, f32)>,
    vel: (f32, f32),
    tilt: f32,
    /// When the pointer last moved, and the opacity now.
    moved: Instant,
    last_target: (f32, f32),
    opacity: f32,
    shape: Shape,
    textures: Option<Textures>,
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
        let (a, i, x) = (arrow(), ibeam(), crosshair());
        let rgb = rgb_of(color);
        Some(Textures {
            effect,
            image: sys::gs_effect_get_param_by_name(effect, c"image".as_ptr()),
            opacity: sys::gs_effect_get_param_by_name(effect, c"opacity".as_ptr()),
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
    }
}

unsafe fn read_look(settings: *mut sys::obs_data_t) -> Look {
    let int = |k: &std::ffi::CStr| unsafe { sys::obs_data_get_int(settings, k.as_ptr()) };
    let float = |k: &std::ffi::CStr| unsafe { sys::obs_data_get_double(settings, k.as_ptr()) } as f32;
    let flag = |k: &std::ffi::CStr| unsafe { sys::obs_data_get_bool(settings, k.as_ptr()) };
    Look {
        style: int(c"style"),
        size: float(c"size").clamp(0.25, 6.0),
        smoothing: float(c"smoothing").clamp(0.0, 1.0),
        tilt: flag(c"tilt"),
        idle: float(c"idle_s").max(0.0),
        clicks: int(c"clicks"),
        arrow_only: flag(c"arrow_only"),
        color: int(c"color") as u32,
    }
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
    }
}

unsafe extern "C" fn create(settings: *mut sys::obs_data_t, context: *mut sys::obs_source_t) -> *mut c_void {
    clicks_join();
    let f = Filter {
        context,
        look: unsafe { read_look(settings) },
        pos: None,
        vel: (0.0, 0.0),
        tilt: 0.0,
        moved: Instant::now(),
        last_target: (f32::NAN, f32::NAN),
        opacity: 1.0,
        shape: Shape::Arrow,
        textures: None,
    };
    Box::into_raw(Box::new(f)) as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let f = unsafe { Box::from_raw(data as *mut Filter) };
    if let Some(t) = f.textures {
        unsafe {
            sys::obs_enter_graphics();
            free_textures(t);
            sys::obs_leave_graphics();
        }
    }
    clicks_leave();
}

unsafe extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    let f = unsafe { &mut *(data as *mut Filter) };
    f.look = unsafe { read_look(settings) };
}

/// One step of a critically damped spring towards `target` (implicit, stable at any `dt`).
fn spring(x: f32, v: f32, target: f32, omega: f32, dt: f32) -> (f32, f32) {
    let f = 1.0 + 2.0 * dt * omega;
    let oo = omega * omega;
    let (hoo, hhoo) = (dt * oo, dt * dt * oo);
    let inv = 1.0 / (f + hhoo);
    ((f * x + dt * v + hhoo * target) * inv, (v + hoo * (target - x)) * inv)
}

unsafe extern "C" fn tick(data: *mut c_void, seconds: f32) {
    let f = unsafe { &mut *(data as *mut Filter) };
    let dt = seconds.clamp(1e-4, 0.1);
    let target = input::pointer();
    if target != f.last_target {
        f.last_target = target;
        f.moved = Instant::now();
    }
    // smoothing 0: stiff (follows at once); 1: a slow glide
    let omega = 60.0 - 52.0 * f.look.smoothing;
    let (x, y) = f.pos.unwrap_or(target);
    let (nx, vx) = spring(x, f.vel.0, target.0, omega, dt);
    let (ny, vy) = spring(y, f.vel.1, target.1, omega, dt);
    f.pos = Some((nx, ny));
    f.vel = (vx, vy);
    let want_tilt = if f.look.tilt { (vx * 0.012).clamp(-14.0, 14.0) } else { 0.0 };
    f.tilt += (want_tilt - f.tilt) * (dt * 12.0).min(1.0);
    let idle = f.look.idle > 0.0 && f.moved.elapsed().as_secs_f32() > f.look.idle;
    let want = if idle { 0.0 } else { 1.0 };
    f.opacity += (want - f.opacity) * (dt * 8.0).min(1.0);
    f.shape = if f.look.arrow_only { Shape::Arrow } else { input::shape() };
}

/// Draws `tex` with its hot spot at `at`, `size` pixels square, turned by `angle` degrees.
unsafe fn draw(t: &Textures, tex: *mut sys::gs_texture_t, at: (f32, f32), size: f32, hot: (f32, f32), angle: f32, opacity: f32) {
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
    unsafe { sys::obs_source_skip_video_filter(f.context) };
    let target = unsafe { sys::obs_filter_get_target(f.context) };
    if target.is_null() {
        return;
    }
    let (tw, th) = unsafe { (sys::obs_source_get_base_width(target), sys::obs_source_get_base_height(target)) };
    let Some((px, py)) = f.pos else { return };
    if tw == 0 || th == 0 {
        return;
    }
    if f.textures.as_ref().is_some_and(|t| t.color != f.look.color)
        && let Some(old) = f.textures.take()
    {
        unsafe { free_textures(old) };
    }
    if f.textures.is_none() {
        f.textures = unsafe { make_textures(f.look.color) };
    }
    let Some(t) = &f.textures else { return };
    // display points → the capture's pixels
    let (dx, dy, dw, _) = input::main_display();
    let scale = if dw > 0.0 { tw as f32 / dw } else { 1.0 };
    let (x, y) = ((px - dx) * scale, (py - dy) * scale);
    let size = POINTS * f.look.size * scale;
    unsafe {
        sys::gs_blend_state_push();
        sys::gs_enable_blending(true);
        sys::gs_blend_function(sys::GS_BLEND_ONE, sys::GS_BLEND_INVSRCALPHA);
        let tech = sys::gs_effect_get_technique(t.effect, c"Draw".as_ptr());
        sys::gs_technique_begin(tech);
        sys::gs_technique_begin_pass(tech, 0);
        // clicks under the pointer
        if f.look.clicks > 0 {
            let now = Instant::now();
            for (at, cx, cy) in recent_clicks() {
                let k = (now.duration_since(at).as_secs_f32() / CLICK.as_secs_f32()).clamp(0.0, 1.0);
                let (cx, cy) = ((cx - dx) * scale, (cy - dy) * scale);
                let (tex, grow, fade) = if f.look.clicks == 1 { (t.click_disc, 0.9 + 0.3 * k, 0.45) } else { (t.click_ring, 0.6 + 1.4 * k, 0.9) };
                draw(t, tex, (cx, cy), size * 1.6 * grow, (0.5, 0.5), 0.0, fade * (1.0 - k) * f.opacity);
            }
        }
        let (tex, hot, angle) = match (f.look.style, f.shape) {
            (1, _) => (t.dot, (0.5, 0.5), 0.0),
            (2, _) => (t.ring, (0.5, 0.5), 0.0),
            (_, Shape::IBeam) => (t.ibeam.0, t.ibeam.1, 0.0),
            (_, Shape::Crosshair) => (t.cross.0, t.cross.1, 0.0),
            _ => (t.arrow.0, t.arrow.1, f.tilt),
        };
        draw(t, tex, (x, y), size, hot, angle, f.opacity);
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
        info.id = c"lightair_cursor".as_ptr();
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spring_settles_without_overshoot() {
        let (mut x, mut v) = (0.0f32, 0.0f32);
        let mut max = 0.0f32;
        for _ in 0..240 {
            (x, v) = spring(x, v, 100.0, 20.0, 1.0 / 60.0);
            max = max.max(x);
        }
        assert!((x - 100.0).abs() < 0.5, "{x}");
        assert!(max <= 100.01, "critically damped: no overshoot ({max})");
    }

    #[test]
    fn shapes_are_drawn_with_their_tip_at_the_hot_spot() {
        let a = arrow();
        // the tip pixel is dark (edge), the body white, the far corner empty
        let px = |x: f32, y: f32| {
            let i = (((y * RES as f32) as usize) * RES + (x * RES as f32) as usize) * 4;
            [a.rgba[i], a.rgba[i + 1], a.rgba[i + 2], a.rgba[i + 3]]
        };
        assert!(px(a.hot.0 + 0.01, a.hot.1 + 0.02)[3] > 0, "something at the tip");
        assert_eq!(px(0.2, 0.4), [255, 255, 255, 255], "white body");
        assert_eq!(px(0.95, 0.1)[3], 0, "nothing far from it");
    }
}
