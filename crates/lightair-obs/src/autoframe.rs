//! Filters that follow the face the Lightair Effect Stack's tracker sees (docs/13, "Built-in
//! replacements"): **Auto-frame** (`lightair_autoframe`) crops in smoothly so the face stays
//! framed at a shot size, and **Face blur** (`lightair_face_blur`) pixelates or blurs it. Each
//! copies its picture small every few frames for the tracker (as avatars do) and draws through
//! an effect that samples a part of the picture, so nothing is drawn outside the object.
//!
//! Auto-frame: `shot` (0 close, 1 medium, 2 wide), `speed` (0..1), `max_zoom` (1..4).
//! Face blur: `style` (0 pixelate, 1 blur), `strength` (0..1), `grow` (0..1, the area around it).

use std::ffi::{CStr, c_char, c_void};

use crate::avatar::{FT_MESH, FtFace, face_free, face_get, face_new, face_submit};
use crate::sys;

/// The picture copied for the tracker is at most this wide.
const TRACK_WIDTH: u32 = 320;

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

const FRAME_EFFECT: &str = r#"
uniform texture2d image;
uniform float4 view;
sampler_state def { Filter = Linear; AddressU = Clamp; AddressV = Clamp; };
float4 PSFrame(VertInOut vert_in) : TARGET
{
    return image.Sample(def, view.xy + vert_in.uv * view.zw);
}
technique Draw { pass { vertex_shader = VSDefault(vert_in); pixel_shader = PSFrame(vert_in); } }
"#;

const BLUR_EFFECT: &str = r#"
uniform texture2d image;
uniform float4 face0;
uniform float4 face1;
uniform float4 face2;
uniform float4 face3;
uniform float4 face4;
uniform float4 face5;
uniform float2 texel;
uniform float block;
uniform float style;
sampler_state def { Filter = Linear; AddressU = Clamp; AddressV = Clamp; };
float inside_of(float2 uv, float4 face)
{
    if (face.z <= 0.0) return 0.0;
    float2 c = face.xy + face.zw * 0.5;
    float2 d = (uv - c) / max(face.zw * 0.5, float2(0.0001, 0.0001));
    return 1.0 - smoothstep(0.85, 1.0, length(d));
}
float4 PSBlur(VertInOut vert_in) : TARGET
{
    float2 uv = vert_in.uv;
    float inside = max(max(max(inside_of(uv, face0), inside_of(uv, face1)), max(inside_of(uv, face2), inside_of(uv, face3))),
                       max(inside_of(uv, face4), inside_of(uv, face5)));
    float4 orig = image.Sample(def, uv);
    if (inside <= 0.0) return orig;
    float4 hidden;
    if (style < 0.5) {
        float2 b = texel * block;
        hidden = image.Sample(def, (floor(uv / b) + 0.5) * b);
    } else {
        float4 sum = float4(0.0, 0.0, 0.0, 0.0);
        for (int x = -3; x <= 3; x++) {
            for (int y = -3; y <= 3; y++) {
                sum += image.Sample(def, uv + float2(x, y) * texel * block * 0.5);
            }
        }
        hidden = sum / 49.0;
    }
    return lerp(orig, hidden, inside);
}
technique Draw { pass { vertex_shader = VSDefault(vert_in); pixel_shader = PSBlur(vert_in); } }
"#;

unsafe fn make_effect(code: &str, name: &CStr) -> *mut sys::gs_effect_t {
    let Ok(code) = std::ffi::CString::new(code) else { return std::ptr::null_mut() };
    let mut err: *mut c_char = std::ptr::null_mut();
    let e = unsafe { sys::gs_effect_create(code.as_ptr(), name.as_ptr(), &mut err) };
    if !err.is_null() {
        tracing::warn!(error = %unsafe { CStr::from_ptr(err) }.to_string_lossy(), "face filter effect");
        unsafe { sys::bfree(err as *mut c_void) };
    }
    e
}

fn v4(v: [f32; 4]) -> sys::vec4 {
    sys::vec4 { __bindgen_anon_1: sys::vec4__bindgen_ty_1 { ptr: v } }
}

/// A copy of the picture for Vision: BGRA bytes, width, height, bytes a row.
type Copy = (Vec<u8>, usize, usize, usize);

/// What both filters share: the tracker, the small copy for it, and the face last seen.
struct Watch {
    tracker: *mut c_void,
    texrender: *mut sys::gs_texrender_t,
    stage: *mut sys::gs_stagesurf_t,
    size: (u32, u32),
    waiting: bool,
    frame_id: u64,
    frames: u32,
    /// The face's box in the picture (0..1: x, y, w, h), and how many frames ago it was seen.
    face: Option<[f32; 4]>,
    missing: u32,
    /// Where copies of the picture go too (Apple's Vision, for every face; macOS).
    sink: Option<std::sync::mpsc::SyncSender<Copy>>,
    /// Every face Vision found lately, and when.
    all: std::sync::Arc<std::sync::Mutex<(Vec<[f32; 4]>, std::time::Instant)>>,
}

impl Watch {
    fn new() -> Self {
        let tracker = face_new();
        Watch {
            tracker,
            texrender: std::ptr::null_mut(),
            stage: std::ptr::null_mut(),
            size: (0, 0),
            waiting: false,
            frame_id: 0,
            frames: 0,
            face: None,
            missing: 0,
            sink: None,
            all: std::sync::Arc::new(std::sync::Mutex::new((Vec::new(), std::time::Instant::now()))),
        }
    }

    /// Also finds every face with Apple's Vision, on a thread of its own (macOS).
    fn with_vision() -> Self {
        #[allow(unused_mut)]
        let mut w = Watch::new();
        #[cfg(target_os = "macos")]
        {
            let (tx, rx) = std::sync::mpsc::sync_channel::<Copy>(1);
            let all = w.all.clone();
            std::thread::Builder::new()
                .name("lightair-faces".into())
                .spawn(move || {
                    while let Ok((mut bytes, w, h, stride)) = rx.recv() {
                        let found = crate::vision::faces(&mut bytes, w, h, stride);
                        if let Ok(mut a) = all.lock() {
                            *a = (found, std::time::Instant::now());
                        }
                    }
                })
                .ok();
            w.sink = Some(tx);
        }
        w
    }

    /// Every face seen lately: Vision's, else the tracker's one.
    fn faces(&self) -> Vec<[f32; 4]> {
        let mut faces: Vec<[f32; 4]> =
            self.all.lock().ok().filter(|a| a.1.elapsed().as_millis() < 700).map(|a| a.0.clone()).unwrap_or_default();
        if faces.is_empty()
            && let Some(face) = self.face
        {
            faces.push(face);
        }
        faces
    }

    /// Hands the tracker the copy made last time, reads its face, and copies `target` again
    /// (every third frame). Graphics context held (in a render).
    unsafe fn look(&mut self, picture: *mut sys::gs_texture_t, w: u32, h: u32) {
        if (self.tracker.is_null() && self.sink.is_none()) || w == 0 || h == 0 {
            return;
        }
        self.frames += 1;
        if !self.frames.is_multiple_of(3) {
            return;
        }
        let tw = w.min(TRACK_WIDTH) & !1;
        let th = ((h as u64 * tw as u64 / w as u64) as u32).max(2) & !1;
        unsafe {
            if self.texrender.is_null() {
                self.texrender = sys::gs_texrender_create(sys::GS_BGRA, sys::GS_ZS_NONE);
            }
            if self.size != (tw, th) {
                if !self.stage.is_null() {
                    sys::gs_stagesurface_destroy(self.stage);
                }
                self.stage = sys::gs_stagesurface_create(tw, th, sys::GS_BGRA);
                self.size = (tw, th);
                self.waiting = false;
            }
            if self.waiting {
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut linesize = 0u32;
                if sys::gs_stagesurface_map(self.stage, &mut data, &mut linesize) {
                    if let Some(sink) = &self.sink {
                        let bytes = std::slice::from_raw_parts(data, linesize as usize * th as usize).to_vec();
                        let _ = sink.try_send((bytes, tw as usize, th as usize, linesize as usize));
                    }
                    self.frame_id += 1;
                    let (tracker, id) = (self.tracker, self.frame_id);
                    if !tracker.is_null() {
                        face_submit(tracker, data, linesize, tw, th, id, crate::clock::now_ns());
                    }
                    sys::gs_stagesurface_unmap(self.stage);
                }
                self.waiting = false;
                if !self.tracker.is_null() {
                    self.read_face(tw, th);
                }
            }
            sys::gs_texrender_reset(self.texrender);
            if sys::gs_texrender_begin(self.texrender, tw, th) {
                let clear = v4([0.0, 0.0, 0.0, 1.0]);
                sys::gs_clear(sys::GS_CLEAR_COLOR, &clear, 0.0, 0);
                sys::gs_ortho(0.0, w as f32, 0.0, h as f32, -100.0, 100.0);
                sys::gs_blend_state_push();
                sys::gs_enable_blending(false);
                let default = sys::obs_get_base_effect(sys::OBS_EFFECT_DEFAULT);
                sys::gs_effect_set_texture(sys::gs_effect_get_param_by_name(default, c"image".as_ptr()), picture);
                while sys::gs_effect_loop(default, c"Draw".as_ptr()) {
                    sys::gs_draw_sprite(picture, 0, w, h);
                }
                sys::gs_blend_state_pop();
                sys::gs_texrender_end(self.texrender);
                sys::gs_stage_texture(self.stage, sys::gs_texrender_get_texture(self.texrender));
                self.waiting = true;
            }
        }
    }

    /// The tracker's latest face as a box in the picture.
    fn read_face(&mut self, tw: u32, th: u32) {
        let mut face: Box<FtFace> = unsafe { Box::new(std::mem::zeroed()) };
        let (tracker, id) = (self.tracker, self.frame_id);
        let ok = face_get(tracker, id.saturating_sub(1), &mut face);
        let (s, n) = (face.region_start[FT_MESH].max(0) as usize, face.region_count[FT_MESH].max(0) as usize);
        let mesh = face.pts.get(s..s + n).unwrap_or(&[]);
        if !ok || !face.valid || !face.present || mesh.is_empty() {
            self.missing += 1;
            if self.missing > 30 {
                self.face = None;
            }
            return;
        }
        self.missing = 0;
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for p in mesh {
            x0 = x0.min(p[0]);
            y0 = y0.min(p[1]);
            x1 = x1.max(p[0]);
            y1 = y1.max(p[1]);
        }
        let (tw, th) = (tw as f32, th as f32);
        self.face = Some([x0 / tw, y0 / th, (x1 - x0) / tw, (y1 - y0) / th]);
    }

    unsafe fn free(&mut self) {
        unsafe {
            if !self.stage.is_null() {
                sys::gs_stagesurface_destroy(self.stage);
            }
            if !self.texrender.is_null() {
                sys::gs_texrender_destroy(self.texrender);
            }
        }
        if !self.tracker.is_null() {
            face_free(self.tracker);
        }
    }
}

/// The picture under the filter, drawn into `tr` (the filters' own copy; drawing the source
/// itself from its own filter is skipped by libobs).
unsafe fn picture(context: *mut sys::obs_source_t, tr: &mut *mut sys::gs_texrender_t, w: u32, h: u32) -> *mut sys::gs_texture_t {
    unsafe {
        if tr.is_null() {
            *tr = sys::gs_texrender_create(sys::GS_RGBA, sys::GS_ZS_NONE);
        }
        if tr.is_null() {
            return std::ptr::null_mut();
        }
        sys::gs_texrender_reset(*tr);
        if !sys::gs_texrender_begin(*tr, w, h) {
            return std::ptr::null_mut();
        }
        let clear = v4([0.0; 4]);
        sys::gs_clear(sys::GS_CLEAR_COLOR, &clear, 0.0, 0);
        sys::gs_ortho(0.0, w as f32, 0.0, h as f32, -100.0, 100.0);
        if sys::obs_source_process_filter_begin(context, sys::GS_RGBA, sys::OBS_NO_DIRECT_RENDERING) {
            sys::gs_blend_state_push();
            sys::gs_blend_function(sys::GS_BLEND_ONE, sys::GS_BLEND_ZERO);
            sys::obs_source_process_filter_end(context, sys::obs_get_base_effect(sys::OBS_EFFECT_DEFAULT), w, h);
            sys::gs_blend_state_pop();
        }
        sys::gs_texrender_end(*tr);
        sys::gs_texrender_get_texture(*tr)
    }
}

/// Draws `tex` at `w`×`h` through `effect` (its `image` set to it).
unsafe fn draw_through(effect: *mut sys::gs_effect_t, tex: *mut sys::gs_texture_t, w: u32, h: u32) {
    unsafe {
        sys::gs_effect_set_texture(sys::gs_effect_get_param_by_name(effect, c"image".as_ptr()), tex);
        sys::gs_blend_state_push();
        sys::gs_enable_blending(true);
        sys::gs_blend_function(sys::GS_BLEND_ONE, sys::GS_BLEND_INVSRCALPHA);
        while sys::gs_effect_loop(effect, c"Draw".as_ptr()) {
            sys::gs_draw_sprite(tex, 0, w, h);
        }
        sys::gs_blend_state_pop();
    }
}

unsafe fn target_size(context: *mut sys::obs_source_t) -> (*mut sys::obs_source_t, u32, u32) {
    let target = unsafe { sys::obs_filter_get_target(context) };
    if target.is_null() {
        return (target, 0, 0);
    }
    unsafe { (target, sys::obs_source_get_base_width(target), sys::obs_source_get_base_height(target)) }
}

// ---------------------------------------------------------------------------------------------
// Auto-frame

struct AutoFrame {
    context: *mut sys::obs_source_t,
    effect: *mut sys::gs_effect_t,
    full: *mut sys::gs_texrender_t,
    watch: Option<Watch>,
    shot: i64,
    speed: f32,
    max_zoom: f32,
    /// The view as it moves (0..1: x, y, w, h).
    view: [f32; 4],
}

unsafe extern "C" fn af_name(_: *mut c_void) -> *const c_char {
    c"Auto-frame".as_ptr()
}

unsafe extern "C" fn af_update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    let f = unsafe { &mut *(data as *mut AutoFrame) };
    f.shot = unsafe { sys::obs_data_get_int(settings, c"shot".as_ptr()) };
    f.speed = (unsafe { sys::obs_data_get_double(settings, c"speed".as_ptr()) } as f32).clamp(0.05, 1.0);
    f.max_zoom = (unsafe { sys::obs_data_get_double(settings, c"max_zoom".as_ptr()) } as f32).clamp(1.0, 4.0);
}

unsafe extern "C" fn af_defaults(settings: *mut sys::obs_data_t) {
    unsafe {
        sys::obs_data_set_default_int(settings, c"shot".as_ptr(), 1);
        sys::obs_data_set_default_double(settings, c"speed".as_ptr(), 0.4);
        sys::obs_data_set_default_double(settings, c"max_zoom".as_ptr(), 2.5);
    }
}

unsafe extern "C" fn af_create(settings: *mut sys::obs_data_t, context: *mut sys::obs_source_t) -> *mut c_void {
    let mut f = Box::new(AutoFrame {
        context,
        effect: std::ptr::null_mut(),
        full: std::ptr::null_mut(),
        watch: None,
        shot: 1,
        speed: 0.4,
        max_zoom: 2.5,
        view: [0.0, 0.0, 1.0, 1.0],
    });
    unsafe { af_update(&mut *f as *mut AutoFrame as *mut c_void, settings) };
    Box::into_raw(f) as *mut c_void
}

unsafe extern "C" fn af_destroy(data: *mut c_void) {
    let mut f = unsafe { Box::from_raw(data as *mut AutoFrame) };
    unsafe {
        sys::obs_enter_graphics();
        if let Some(w) = f.watch.as_mut() {
            w.free();
        }
        if !f.effect.is_null() {
            sys::gs_effect_destroy(f.effect);
        }
        if !f.full.is_null() {
            sys::gs_texrender_destroy(f.full);
        }
        sys::obs_leave_graphics();
    }
}

/// The view that frames `face` (a box in the picture) at `shot`, kept inside the picture.
fn framing(face: Option<[f32; 4]>, shot: i64, max_zoom: f32, aspect: f32) -> [f32; 4] {
    let Some([fx, fy, fw, fh]) = face else { return [0.0, 0.0, 1.0, 1.0] };
    // how much of the frame's height the face fills
    let fill = match shot {
        0 => 0.45,
        2 => 0.16,
        _ => 0.28,
    };
    let zoom = (fill / fh.max(0.01)).clamp(1.0, max_zoom);
    let (vw, vh) = (1.0 / zoom, 1.0 / zoom);
    let _ = aspect;
    // the face a little above the middle (headroom)
    let (cx, cy) = (fx + fw * 0.5, fy + fh * 0.5 + vh * 0.08);
    let x = (cx - vw / 2.0).clamp(0.0, 1.0 - vw);
    let y = (cy - vh / 2.0).clamp(0.0, 1.0 - vh);
    [x, y, vw, vh]
}

unsafe extern "C" fn af_tick(data: *mut c_void, seconds: f32) {
    let f = unsafe { &mut *(data as *mut AutoFrame) };
    // the biggest face (the person in front)
    let face = f.watch.as_ref().and_then(|w| w.faces().into_iter().max_by(|a, b| (a[2] * a[3]).total_cmp(&(b[2] * b[3]))));
    let want = framing(face, f.shot, f.max_zoom, 16.0 / 9.0);
    // eases there; small moves inside a dead zone are left alone (no wobble)
    let k = (seconds * (0.6 + 4.0 * f.speed)).min(1.0);
    let far = (0..4).map(|i| (want[i] - f.view[i]).abs()).fold(0.0f32, f32::max);
    if far > 0.015 {
        for (v, w) in f.view.iter_mut().zip(want) {
            *v += (w - *v) * k;
        }
    }
}

unsafe extern "C" fn af_render(data: *mut c_void, _: *mut sys::gs_effect_t) {
    let f = unsafe { &mut *(data as *mut AutoFrame) };
    let (_, w, h) = unsafe { target_size(f.context) };
    if w == 0 || h == 0 {
        unsafe { sys::obs_source_skip_video_filter(f.context) };
        return;
    }
    unsafe {
        if f.effect.is_null() {
            f.effect = make_effect(&format!("{VERTEX}{FRAME_EFFECT}"), c"lightair-autoframe.effect");
        }
        let tex = picture(f.context, &mut f.full, w, h);
        if f.effect.is_null() || tex.is_null() {
            return;
        }
        f.watch.get_or_insert_with(Watch::with_vision).look(tex, w, h);
        let v = v4(f.view);
        sys::gs_effect_set_vec4(sys::gs_effect_get_param_by_name(f.effect, c"view".as_ptr()), &v);
        draw_through(f.effect, tex, w, h);
    }
}

// ---------------------------------------------------------------------------------------------
// Face blur

struct FaceBlur {
    context: *mut sys::obs_source_t,
    effect: *mut sys::gs_effect_t,
    full: *mut sys::gs_texrender_t,
    watch: Option<Watch>,
    style: i64,
    strength: f32,
    grow: f32,
}

unsafe extern "C" fn fb_name(_: *mut c_void) -> *const c_char {
    c"Face blur".as_ptr()
}

unsafe extern "C" fn fb_update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    let f = unsafe { &mut *(data as *mut FaceBlur) };
    f.style = unsafe { sys::obs_data_get_int(settings, c"style".as_ptr()) };
    f.strength = (unsafe { sys::obs_data_get_double(settings, c"strength".as_ptr()) } as f32).clamp(0.0, 1.0);
    f.grow = (unsafe { sys::obs_data_get_double(settings, c"grow".as_ptr()) } as f32).clamp(0.0, 1.0);
}

unsafe extern "C" fn fb_defaults(settings: *mut sys::obs_data_t) {
    unsafe {
        sys::obs_data_set_default_double(settings, c"strength".as_ptr(), 0.6);
        sys::obs_data_set_default_double(settings, c"grow".as_ptr(), 0.3);
    }
}

unsafe extern "C" fn fb_create(settings: *mut sys::obs_data_t, context: *mut sys::obs_source_t) -> *mut c_void {
    let mut f = Box::new(FaceBlur {
        context,
        effect: std::ptr::null_mut(),
        full: std::ptr::null_mut(),
        watch: None,
        style: 0,
        strength: 0.6,
        grow: 0.3,
    });
    unsafe { fb_update(&mut *f as *mut FaceBlur as *mut c_void, settings) };
    Box::into_raw(f) as *mut c_void
}

unsafe extern "C" fn fb_destroy(data: *mut c_void) {
    let mut f = unsafe { Box::from_raw(data as *mut FaceBlur) };
    unsafe {
        sys::obs_enter_graphics();
        if let Some(w) = f.watch.as_mut() {
            w.free();
        }
        if !f.effect.is_null() {
            sys::gs_effect_destroy(f.effect);
        }
        if !f.full.is_null() {
            sys::gs_texrender_destroy(f.full);
        }
        sys::obs_leave_graphics();
    }
}

unsafe extern "C" fn fb_render(data: *mut c_void, _: *mut sys::gs_effect_t) {
    let f = unsafe { &mut *(data as *mut FaceBlur) };
    let (_, w, h) = unsafe { target_size(f.context) };
    if w == 0 || h == 0 {
        unsafe { sys::obs_source_skip_video_filter(f.context) };
        return;
    }
    unsafe {
        let watch = f.watch.get_or_insert_with(Watch::with_vision);
        let tex = picture(f.context, &mut f.full, w, h);
        if tex.is_null() {
            return;
        }
        watch.look(tex, w, h);
        let faces = watch.faces();
        if f.effect.is_null() {
            f.effect = make_effect(&format!("{VERTEX}{BLUR_EFFECT}"), c"lightair-face-blur.effect");
        }
        if f.effect.is_null() {
            return;
        }
        // each face's box, grown (hair, ears, chin)
        let g = 1.0 + f.grow;
        let p = |n: &CStr| sys::gs_effect_get_param_by_name(f.effect, n.as_ptr());
        let names = [c"face0", c"face1", c"face2", c"face3", c"face4", c"face5"];
        for (i, name) in names.iter().enumerate() {
            let face = match faces.get(i) {
                Some([x, y, fw, fh]) => {
                    let (cx, cy) = (x + fw / 2.0, y + fh / 2.0);
                    let (gw, gh) = (fw * g * 1.1, fh * g * 1.25);
                    v4([cx - gw / 2.0, cy - gh / 2.0, gw, gh])
                }
                None => v4([0.0; 4]),
            };
            sys::gs_effect_set_vec4(p(name), &face);
        }
        let texel = sys::vec2 { __bindgen_anon_1: sys::vec2__bindgen_ty_1 { ptr: [1.0 / w as f32, 1.0 / h as f32] } };
        sys::gs_effect_set_vec2(p(c"texel"), &texel);
        sys::gs_effect_set_float(p(c"block"), 4.0 + f.strength * (w.min(h) as f32 / 18.0));
        sys::gs_effect_set_float(p(c"style"), f.style as f32);
        draw_through(f.effect, tex, w, h);
    }
}

/// Registers the filters with libobs (once; after startup).
pub fn register() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = c"lightair_autoframe".as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_FILTER;
        info.output_flags = sys::OBS_SOURCE_VIDEO;
        info.get_name = Some(af_name);
        info.create = Some(af_create);
        info.destroy = Some(af_destroy);
        info.update = Some(af_update);
        info.get_defaults = Some(af_defaults);
        info.video_tick = Some(af_tick);
        info.video_render = Some(af_render);
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());

        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = c"lightair_face_blur".as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_FILTER;
        info.output_flags = sys::OBS_SOURCE_VIDEO;
        info.get_name = Some(fb_name);
        info.create = Some(fb_create);
        info.destroy = Some(fb_destroy);
        info.update = Some(fb_update);
        info.get_defaults = Some(fb_defaults);
        info.video_render = Some(fb_render);
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}
