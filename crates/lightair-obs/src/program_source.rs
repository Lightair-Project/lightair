//! macOS, with `LIGHTAIR_ENGINE=own`: `lightair_program`, a private libobs source that draws the
//! newest picture Lightair's own compositor made of a canvas's program (a BGRA IOSurface), and
//! each canvas's output built on it, so libobs's encoders and outputs take the compositor's
//! picture until they're replaced.
//!
//! - **The source:** the compositor hands each finished frame over with [`publish`]; the source
//!   draws the newest one as a libobs texture over its IOSurface (made once, then rebound to each
//!   new surface). It holds the buffer it draws, so the compositor doesn't paint over it.
//! - **Outputs:** every canvas gets a private libobs canvas of its own ([`attach`]) whose only
//!   channel shows a `lightair_program` source for it. Outputs libobs still makes (with
//!   `LIGHTAIR_OUTPUTS=libobs`, or those Lightair can't make yet: `own_outputs`) take that
//!   canvas's video ([`video`]) instead of the canvas's, so they carry
//!   the compositor's picture while the canvas's own channels stay as they are: the transition
//!   on channel 0 and the overlay scenes keep their sources active and showing, captures keep
//!   running, and libobs mixes their sound as before (outputs take libobs's audio, unchanged).
//!   The output canvas isn't active, mixes no sound and holds no scenes.
//! - **Fallback:** an output's source draws only a recent frame; while the compositor has none
//!   for its canvas (starting, stopped, behind, failed) it draws libobs's own picture of the
//!   canvas instead, so an output never goes black or freezes.

use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};


use lightair_model::{Canvas, Id};
use objc2_core_foundation::CFRetained;
use objc2_core_video::{CVPixelBuffer, CVPixelBufferGetHeight, CVPixelBufferGetWidth};
use parking_lot::Mutex;

use crate::bridge::surface_of;
use crate::obs::{CanvasRef, Data, ObsCanvas, ObsSource, SourceRef};
use crate::sys;

pub use lightair_host::program::{
    SceneTarget, add_scene_target, in_use, publish_scene, remove_scene_target, scene_targets,
};

/// The libobs source type.
pub const ID: &CStr = c"lightair_program";

type Buffer = CFRetained<CVPixelBuffer>;

pub use lightair_host::program::{newest, publish, withdraw};

/// A private `lightair_program` source drawing `canvas`'s compositor picture (registered with
/// libobs first if it isn't yet). Engine thread.
pub fn create(canvas: Id) -> Option<SourceRef> {
    register();
    let mut settings = Data::new();
    settings.set_str("canvas", &canvas.to_string());
    ObsSource::create_private(ID.to_str().ok()?, "Lightair program", Some(&settings))?.new_ref()
}

// ------------------------------------------------------------------ outputs

/// A canvas's output: the private libobs canvas the outputs take, showing only `source`.
struct Out {
    canvas: ObsCanvas,
    source: SourceRef,
    /// Width, height and frame rate (numerator, denominator) the output canvas was made at.
    shape: (u32, u32, u32, u32),
    primary: bool,
}

struct Outs(Option<HashMap<Id, Out>>);

/// Never held while waiting on libobs's graphics thread from inside it: the source's drawing
/// doesn't take it.
static OUTS: Mutex<Outs> = Mutex::new(Outs(None));

fn shape(canvas: &Canvas) -> (u32, u32, u32, u32) {
    lightair_host::program::shape_of(canvas)
}

/// Engine thread: makes `canvas`'s output (replacing one it had). `libobs` is the canvas's own
/// libobs canvas, whose picture the output shows while the compositor has none; `primary` when
/// that's libobs's main canvas. The primary canvas's output outlives the document (another
/// workspace opened): it's handed to the new primary canvas, so what records or sends it (the
/// virtual camera, NDI) carries on.
pub(crate) fn attach(canvas: &Canvas, libobs: CanvasRef, primary: bool) {
    if !crate::bridge::own() {
        return;
    }
    register();
    if primary && let Some(old) = adopt(canvas) {
        // Lightair's own outputs on the primary canvas follow it, as libobs's on its output do
        crate::own_outputs::canvas_moved(old, canvas.id);
        return;
    }
    detach(canvas.id);
    make(canvas.id, shape(canvas), libobs, primary);
}

/// Makes `canvas`'s output at `shape` (width, height and frame rate).
fn make(canvas: Id, (w, h, num, den): (u32, u32, u32, u32), libobs: CanvasRef, primary: bool) {
    let Some(out) = ObsCanvas::create_private(&format!("program:{canvas}"), w, h, num, den, 0) else {
        tracing::error!(%canvas, "own engine: no output canvas; the outputs take libobs's picture");
        return;
    };
    let Some(source) = create(canvas) else {
        tracing::error!(%canvas, "own engine: no lightair_program source; the outputs take libobs's picture");
        return;
    };
    let fallback = if primary { Fallback::Main } else { Fallback::Canvas(libobs) };
    configure(&source, Some(Output { size: (w, h), fallback }));
    unsafe { sys::obs_canvas_set_channel(out.as_ptr(), 0, source.as_ptr()) };
    OUTS.lock()
        .0
        .get_or_insert_with(HashMap::new)
        .insert(canvas, Out { canvas: out, source, shape: (w, h, num, den), primary });
    share_shapes();
}

/// An output taken down while libobs's main video is reset, and what makes it again.
pub(crate) struct TakenDown {
    canvas: Id,
    shape: (u32, u32, u32, u32),
    primary: bool,
}

/// Engine thread, before libobs's main video is reset: every output is released and what it was
/// is returned, for [`bring_back`]. Resetting the main video frees the video mixes of canvases
/// libobs keeps off its public list, such as these, without telling them; released first, they
/// go cleanly. `None` while something records or streams from one (libobs refuses the reset
/// then, so nothing is taken down).
pub(crate) fn take_down() -> Option<Vec<TakenDown>> {
    let mut outs = OUTS.lock();
    let Some(map) = outs.0.as_mut() else { return Some(Vec::new()) };
    if map.values().any(|o| o.canvas.video_active()) {
        return None;
    }
    let gone: Vec<(Id, Out)> = map.drain().collect();
    drop(outs);
    share_shapes();
    let mut taken = Vec::with_capacity(gone.len());
    for (canvas, out) in gone {
        taken.push(TakenDown { canvas, shape: out.shape, primary: out.primary });
        release(out);
    }
    Some(taken)
}

/// Engine thread, after the main video's reset (done or not): makes the outputs again, the
/// resized canvas's at its new `size` (`resized`). `libobs` gives each canvas's own libobs
/// canvas, whose picture an output shows while the compositor has none.
pub(crate) fn bring_back(
    taken: Vec<TakenDown>,
    resized: Option<(Id, (u32, u32, u32, u32))>,
    libobs: impl Fn(Id) -> Option<CanvasRef>,
) {
    for t in taken {
        let shape = match resized {
            Some((id, shape)) if id == t.canvas => shape,
            _ => t.shape,
        };
        if let Some(own) = libobs(t.canvas) {
            make(t.canvas, shape, own, t.primary);
        }
    }
}

/// `canvas`'s shape as an output takes it (even sizes, a frame rate of at least 1/1).
pub(crate) fn shape_of(canvas: &Canvas) -> (u32, u32, u32, u32) {
    lightair_host::program::shape_of(canvas)
}

/// The primary canvas's output, kept from the document before, given to `canvas`; the canvas
/// it was given before, if there was one.
fn adopt(canvas: &Canvas) -> Option<Id> {
    let mut guard = OUTS.lock();
    let outs = guard.0.as_mut()?;
    let old = outs.iter().find(|(_, o)| o.primary).map(|(id, _)| *id)?;
    let mut out = outs.remove(&old)?;
    if let Some(p) = program_of(&out.source) {
        *p.canvas.lock() = Some(canvas.id);
    }
    let wanted = shape(canvas);
    let (w, h, num, den) = wanted;
    if out.shape != wanted && out.canvas.try_reset(w, h, num, den) {
        out.shape = wanted;
        set_size(&out.source, (w, h));
    }
    outs.insert(canvas.id, out);
    drop(guard);
    share_shapes();
    Some(old)
}

/// Engine thread: removes `canvas`'s output.
pub(crate) fn detach(canvas: Id) {
    let gone = OUTS.lock().0.as_mut().and_then(|o| o.remove(&canvas));
    // released outside the lock: libobs waits for its graphics thread to let the canvas go
    if let Some(out) = gone {
        release(out);
    }
    share_shapes();
}

/// Engine thread: removes every output but the primary canvas's (another workspace), which
/// shows libobs's picture until a canvas takes it again.
pub(crate) fn detach_all() {
    let gone: Vec<Out> = match OUTS.lock().0.as_mut() {
        Some(outs) => {
            let ids: Vec<Id> = outs.iter().filter(|(_, o)| !o.primary).map(|(id, _)| *id).collect();
            ids.into_iter().filter_map(|id| outs.remove(&id)).collect()
        }
        None => Vec::new(),
    };
    for out in gone {
        release(out);
    }
    share_shapes();
}

/// Engine thread, shutting down (after the outputs have stopped): removes every output.
pub(crate) fn shutdown() {
    let gone = OUTS.lock().0.take();
    for out in gone.into_iter().flat_map(|o| o.into_values()) {
        release(out);
    }
    share_shapes();
}

fn release(out: Out) {
    unsafe { sys::obs_canvas_set_channel(out.canvas.as_ptr(), 0, std::ptr::null_mut()) };
    drop(out);
}

/// Engine thread: gives `canvas`'s output the canvas's size and frame rate. False while
/// something records or streams from it (it keeps its size, as libobs's canvases do).
pub(crate) fn resize(canvas: &Canvas) -> bool {
    let mut outs = OUTS.lock();
    let Some(out) = outs.0.as_mut().and_then(|o| o.get_mut(&canvas.id)) else { return true };
    let wanted = shape(canvas);
    if out.shape == wanted {
        return true;
    }
    let (w, h, num, den) = wanted;
    if !out.canvas.try_reset(w, h, num, den) {
        return false;
    }
    out.shape = wanted;
    set_size(&out.source, (w, h));
    drop(outs);
    share_shapes();
    true
}

/// Whether something libobs makes records or streams from `canvas`'s output
/// (`lightair_host::program::in_use` asks). Any thread (waits for libobs's graphics thread to finish a frame at worst; never
/// call it from that thread).
fn libobs_in_use(canvas: Id) -> bool {
    OUTS.lock().0.as_ref().and_then(|o| o.get(&canvas)).is_some_and(|out| out.canvas.video_active())
}

/// The canvas whose output gives `video` (`None`: the primary canvas's), if it's one.
pub(crate) fn canvas_of_video(video: Option<*mut sys::video_t>) -> Option<Id> {
    let outs = OUTS.lock();
    let outs = outs.0.as_ref()?;
    match video {
        None => outs.iter().find(|(_, o)| o.primary).map(|(id, _)| *id),
        Some(v) => outs
            .iter()
            .find(|(_, o)| unsafe { sys::obs_canvas_get_video(o.canvas.as_ptr()) } == v)
            .map(|(id, _)| *id),
    }
}

/// Tells Lightair's outputs the shape of each canvas's output (they size their encoders by it
/// when the canvas is gone from the document: another workspace opened).
fn share_shapes() {
    let shapes: Vec<(Id, (u32, u32, u32, u32))> =
        OUTS.lock().0.as_ref().map(|o| o.iter().map(|(id, out)| (*id, out.shape)).collect()).unwrap_or_default();
    lightair_host::program::set_output_shapes(shapes);
}

/// The video the outputs of `canvas` take (`None`: the primary canvas), if it has an output.
pub(crate) fn video(canvas: Option<Id>) -> Option<*mut sys::video_t> {
    let outs = OUTS.lock();
    let outs = outs.0.as_ref()?;
    let out = match canvas {
        Some(id) => outs.get(&id)?,
        None => outs.values().find(|o| o.primary)?,
    };
    let video = unsafe { sys::obs_canvas_get_video(out.canvas.as_ptr()) };
    (!video.is_null()).then_some(video)
}

// ------------------------------------------------------------------ the libobs source

/// What the source shows: the buffer it draws and a libobs texture over its surface.
struct Shown {
    buffer: Option<Buffer>,
    tex: *mut sys::gs_texture_t,
    size: (u32, u32),
}

/// libobs's own picture of a canvas.
enum Fallback {
    /// The main canvas: its texture, already drawn this frame (libobs draws it first).
    Main,
    /// Another canvas: its channels drawn again.
    Canvas(CanvasRef),
}

/// A source made for a canvas's output.
struct Output {
    /// The output canvas's size: what the source draws at.
    size: (u32, u32),
    fallback: Fallback,
}

struct Program {
    canvas: Mutex<Option<Id>>,
    shown: Mutex<Shown>,
    /// Set for a canvas's output: only recent frames are drawn, libobs's picture otherwise.
    output: Mutex<Option<Output>>,
}

// SAFETY: the texture is only touched on libobs's graphics thread (or inside its context when
// the source is destroyed); the pixel buffers and canvas reference are reference counted
// thread-safely.
unsafe impl Send for Program {}
unsafe impl Sync for Program {}

fn program<'a>(data: *mut c_void) -> &'a Program {
    unsafe { &*(data as *const Program) }
}

/// The source's own state (`None` for a source of another type, or one that failed to make).
fn program_of(source: &SourceRef) -> Option<&Program> {
    let id = unsafe { sys::obs_source_get_id(source.as_ptr()) };
    if id.is_null() || unsafe { CStr::from_ptr(id) } != ID {
        return None;
    }
    let data = unsafe { sys::obs_obj_get_data(source.as_ptr().cast()) };
    (!data.is_null()).then(|| program(data))
}

fn configure(source: &SourceRef, output: Option<Output>) {
    if let Some(p) = program_of(source) {
        *p.output.lock() = output;
    }
}

fn set_size(source: &SourceRef, size: (u32, u32)) {
    if let Some(p) = program_of(source)
        && let Some(o) = p.output.lock().as_mut()
    {
        o.size = size;
    }
}

fn canvas_of(settings: *mut sys::obs_data_t) -> Option<Id> {
    let s = unsafe { sys::obs_data_get_string(settings, c"canvas".as_ptr()) };
    if s.is_null() {
        return None;
    }
    uuid::Uuid::parse_str(&unsafe { CStr::from_ptr(s) }.to_string_lossy()).ok().map(Id)
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"Lightair program".as_ptr()
}

unsafe extern "C" fn create_cb(settings: *mut sys::obs_data_t, _source: *mut sys::obs_source_t) -> *mut c_void {
    let p = Program {
        canvas: Mutex::new(canvas_of(settings)),
        shown: Mutex::new(Shown { buffer: None, tex: std::ptr::null_mut(), size: (0, 0) }),
        output: Mutex::new(None),
    };
    Box::into_raw(Box::new(p)) as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let p = unsafe { Box::from_raw(data as *mut Program) };
    let mut shown = p.shown.lock();
    if !shown.tex.is_null() {
        unsafe {
            sys::obs_enter_graphics();
            sys::gs_texture_destroy(shown.tex);
            sys::obs_leave_graphics();
        }
        shown.tex = std::ptr::null_mut();
    }
    shown.buffer = None;
}

unsafe extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    *program(data).canvas.lock() = canvas_of(settings);
}

unsafe extern "C" fn video_render(data: *mut c_void, _effect: *mut sys::gs_effect_t) {
    let p = program(data);
    let Some(canvas) = *p.canvas.lock() else { return };
    let output = p.output.lock();
    let mut shown = p.shown.lock();
    let newest = newest(canvas, output.is_some());
    let changed = match (&newest, &shown.buffer) {
        (Some(n), Some(s)) => CFRetained::as_ptr(n) != CFRetained::as_ptr(s),
        (Some(_), None) => true,
        (None, _) => false,
    };
    if newest.is_none()
        && let Some(out) = output.as_ref()
    {
        // no recent frame: the buffer goes back to the compositor, libobs's picture is drawn
        shown.buffer = None;
        drop(shown);
        unsafe { draw_libobs(out) };
        return;
    }
    if changed && let Some(buffer) = newest {
        let size = (CVPixelBufferGetWidth(&buffer) as u32, CVPixelBufferGetHeight(&buffer) as u32);
        let Some(surface) = surface_of(&buffer) else { return };
        unsafe {
            // the texture follows the newest surface: rebound while the size holds, else made again
            let rebound =
                !shown.tex.is_null() && shown.size == size && sys::gs_texture_rebind_iosurface(shown.tex, surface);
            if !rebound {
                if !shown.tex.is_null() {
                    sys::gs_texture_destroy(shown.tex);
                }
                shown.tex = sys::gs_texture_create_from_iosurface(surface);
            }
        }
        shown.size = size;
        shown.buffer = Some(buffer);
    }
    if shown.tex.is_null() || shown.buffer.is_none() {
        return;
    }
    let (w, h) = output.as_ref().map_or_else(|| canvas_size(shown.size), |o| o.size);
    unsafe {
        // an IOSurface is a rectangle texture on OpenGL (Metal has none)
        let rect = sys::gs_get_device_type() as u32 != sys::GS_DEVICE_METAL;
        let effect =
            sys::obs_get_base_effect(if rect { sys::OBS_EFFECT_DEFAULT_RECT } else { sys::OBS_EFFECT_DEFAULT });
        let image = sys::gs_effect_get_param_by_name(effect, c"image".as_ptr());
        sys::gs_effect_set_texture(image, shown.tex);
        // the frame as it is (it's opaque): copied, not blended
        sys::gs_blend_state_push();
        sys::gs_enable_blending(false);
        while sys::gs_effect_loop(effect, c"Draw".as_ptr()) {
            sys::gs_draw_sprite(shown.tex, 0, w, h);
        }
        sys::gs_blend_state_pop();
    }
}

/// Draws libobs's own picture of an output's canvas at the output's size. Graphics thread.
unsafe fn draw_libobs(out: &Output) {
    let (w, h) = out.size;
    unsafe {
        match &out.fallback {
            Fallback::Main => {
                let tex = sys::obs_get_main_texture();
                if tex.is_null() {
                    return;
                }
                // as libobs draws one canvas's texture into another
                let effect = sys::obs_get_base_effect(sys::OBS_EFFECT_DEFAULT);
                let image = sys::gs_effect_get_param_by_name(effect, c"image".as_ptr());
                sys::gs_effect_set_texture_srgb(image, tex);
                let srgb = sys::gs_framebuffer_srgb_enabled();
                sys::gs_enable_framebuffer_srgb(true);
                sys::gs_blend_state_push();
                sys::gs_enable_blending(false);
                while sys::gs_effect_loop(effect, c"Draw".as_ptr()) {
                    sys::gs_draw_sprite(tex, 0, w, h);
                }
                sys::gs_blend_state_pop();
                sys::gs_enable_framebuffer_srgb(srgb);
            }
            Fallback::Canvas(canvas) => {
                let mut ovi = sys::obs_video_info::default();
                if !sys::obs_canvas_get_video_info(canvas.as_ptr(), &mut ovi)
                    || ovi.base_width == 0
                    || ovi.base_height == 0
                {
                    return;
                }
                sys::gs_matrix_push();
                sys::gs_matrix_scale3f(w as f32 / ovi.base_width as f32, h as f32 / ovi.base_height as f32, 1.0);
                sys::obs_canvas_render(canvas.as_ptr());
                sys::gs_matrix_pop();
            }
        }
    }
}

/// The size a standalone source draws at: libobs's main canvas, else the picture's own.
fn canvas_size(picture: (u32, u32)) -> (u32, u32) {
    let mut ovi = sys::obs_video_info::default();
    if unsafe { sys::obs_get_video_info(&mut ovi) } && ovi.base_width > 0 && ovi.base_height > 0 {
        (ovi.base_width, ovi.base_height)
    } else {
        picture
    }
}

fn size_of(p: &Program) -> (u32, u32) {
    if let Some(o) = p.output.lock().as_ref() {
        return o.size;
    }
    canvas_size(p.shown.lock().size)
}

unsafe extern "C" fn get_width(data: *mut c_void) -> u32 {
    size_of(program(data)).0
}

unsafe extern "C" fn get_height(data: *mut c_void) -> u32 {
    size_of(program(data)).1
}

/// Registers the source type with libobs (once; after startup). Hidden from the source types
/// the app lists.
pub fn register() {
    static DONE: std::sync::Once = std::sync::Once::new();
    lightair_host::program::set_libobs_in_use(libobs_in_use);
    DONE.call_once(|| unsafe {
        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = ID.as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_INPUT;
        info.output_flags = sys::OBS_SOURCE_VIDEO
            | sys::OBS_SOURCE_CUSTOM_DRAW
            | sys::OBS_SOURCE_DO_NOT_DUPLICATE
            | sys::OBS_SOURCE_CAP_DISABLED;
        info.get_name = Some(get_name);
        info.create = Some(create_cb);
        info.destroy = Some(destroy);
        info.update = Some(update);
        info.video_render = Some(video_render);
        info.get_width = Some(get_width);
        info.get_height = Some(get_height);
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}
