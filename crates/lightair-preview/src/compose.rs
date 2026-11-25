//! macOS, with `LIGHTAIR_ENGINE=own`: each canvas's program drawn by Lightair's own compositor
//! (`lightair-render`'s `SceneRenderer`) instead of libobs, for the canvas's previews and its
//! outputs.
//!
//! A hub thread follows the document's canvases and runs one compositor thread per canvas, at
//! the canvas's frame rate. A canvas's compositor draws while one of the canvas's previews shows
//! its program ([`watch`]) or something records or streams from the canvas's output
//! (`program_source::in_use`, asked by the hub a few times a second), and idles otherwise, so it
//! keeps drawing for a recording while the window's preview is hidden.
//!
//! Each frame it reads the document the engine last applied (`bridge::snapshot`), tells the
//! bridge which sources the program and the show's overlays show, takes their newest pictures
//! from it (IOSurfaces libobs drew them into), and draws the program (or the switch under way)
//! on the GPU, then the overlays ("Show over every scene") over it, as libobs draws its overlay
//! channels over what's on air. The frame is copied into a BGRA IOSurface from a small ring: the
//! previews show that buffer as they show libobs's Metal frames (the window's layer takes it as
//! it is, GPUI's `surface()` an NV12 copy), and the canvas's `lightair_program` sources draw it
//! for libobs's outputs.
//!
//! libobs draws each source through its own filters, so the renderer is given the document
//! without the sources' effects (they'd be drawn twice). It does the rest: position, fit, crop,
//! turn, flip, tilt, shape, opacity, and the switch between scenes.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, OnceLock, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lightair_engine::{bridge, program_source};
use lightair_model::{Canvas, Id, Scene, Source, TransitionKind, Workspace};
use lightair_render::SharedGpu;
use lightair_render::media::Picture;
use lightair_render::scene::{Pictures, SceneRenderer, Shot};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_video::{CVPixelBufferGetIOSurface, kCVPixelFormatType_32BGRA};
use objc2_metal::{MTLDevice, MTLPixelFormat, MTLTexture, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage};
use parking_lot::Mutex;

use crate::mac::{Frame, Shared, pixel_buffer};

/// Output buffers at most: the newest (the previews' and libobs's), the one before it (a window
/// may still be showing it), one being drawn, and spares while something holds on to older ones.
const MOST: usize = 6;
/// Buffers not drawn into again for this many frames after they're handed on (a window's layer
/// shows a surface without holding its buffer).
const RECENT: usize = 2;
/// The longest a frame's GPU work is waited for before the frame is dropped.
const GPU_WAIT: Duration = Duration::from_secs(1);
/// How often the hub looks for canvases added or removed and outputs started or stopped.
const HUB_PERIOD: Duration = Duration::from_millis(100);
/// Overlay scenes drawn at most, bottom first (as many as libobs has overlay channels for).
const OVERLAYS: usize = 8;

// ---------------------------------------------------------------- the hub

/// The canvas a preview shows.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// The primary canvas (libobs's main canvas), whichever it is.
    Primary,
    /// Another canvas, by its id as text (the name the engine gives its libobs canvas).
    Canvas(String),
}

struct Viewer {
    target: Target,
    shared: Weak<Shared>,
}

#[derive(Default)]
struct Hub {
    /// The previews given the compositors' frames.
    viewers: Vec<Viewer>,
    /// Canvases something records or streams from.
    in_use: HashSet<Id>,
}

static HUB: LazyLock<Mutex<Hub>> = LazyLock::new(Default::default);

/// Starts the hub (once); false when there's no GPU to draw with.
pub(crate) fn start() -> bool {
    static STARTED: OnceLock<bool> = OnceLock::new();
    *STARTED.get_or_init(|| {
        if let Err(e) = lightair_render::shared_gpu() {
            tracing::error!(%e, "compositor: no GPU; libobs draws the program");
            return false;
        }
        std::thread::Builder::new()
            .name("lightair-compose-hub".into())
            .spawn(run_hub)
            .map_err(|e| tracing::error!(%e, "compositor: no thread"))
            .is_ok()
    })
}

/// Gives `preview` the program frames of the canvas `target` from now on.
pub(crate) fn watch(target: Target, preview: &Arc<Shared>) {
    let mut hub = HUB.lock();
    hub.viewers.retain(|v| !std::ptr::eq(v.shared.as_ptr(), Arc::as_ptr(preview)));
    hub.viewers.push(Viewer { target, shared: Arc::downgrade(preview) });
}

/// Stops giving `preview` frames: libobs draws its program again.
pub(crate) fn unwatch(preview: &Arc<Shared>) {
    HUB.lock().viewers.retain(|v| !std::ptr::eq(v.shared.as_ptr(), Arc::as_ptr(preview)));
    preview.unfeed();
}

/// The previews of `canvas` (`name`: its id as text), and whether something records or streams
/// from it.
fn viewers(canvas: Id, name: &str, primary: bool) -> (Vec<Arc<Shared>>, bool) {
    let hub = HUB.lock();
    let viewers = hub
        .viewers
        .iter()
        .filter(|v| match &v.target {
            Target::Primary => primary,
            Target::Canvas(n) => n == name,
        })
        .filter_map(|v| v.shared.upgrade())
        .collect();
    (viewers, hub.in_use.contains(&canvas))
}

/// The hub's thread: a compositor for each canvas of the document, and which are in use.
fn run_hub() {
    let mut running: HashMap<Id, Compositor> = HashMap::new();
    loop {
        let canvases: Vec<Id> =
            bridge::snapshot().map(|s| s.workspace.canvases.iter().map(|c| c.id).collect()).unwrap_or_default();
        // a canvas removed: its thread stops (and is waited for, outside the hub's lock)
        let gone: Vec<Id> = running.keys().filter(|id| !canvases.contains(id)).copied().collect();
        for id in gone {
            drop(running.remove(&id));
        }
        for id in &canvases {
            if !running.contains_key(id)
                && let Some(c) = Compositor::start(*id)
            {
                running.insert(*id, c);
            }
        }
        let in_use: HashSet<Id> = canvases.iter().copied().filter(|id| program_source::in_use(*id)).collect();
        {
            let mut hub = HUB.lock();
            hub.in_use = in_use;
            hub.viewers.retain(|v| v.shared.strong_count() > 0);
        }
        std::thread::sleep(HUB_PERIOD);
    }
}

// ---------------------------------------------------------------- a canvas's compositor

/// A canvas's compositor thread. Dropping it stops the thread (and waits for it).
struct Compositor {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Compositor {
    /// Starts drawing `canvas`'s program when it's wanted.
    fn start(canvas: Id) -> Option<Compositor> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("lightair-compose".into())
            .spawn({
                let stop = stop.clone();
                move || run(canvas, &stop)
            })
            .map_err(|e| tracing::error!(%e, "compositor: no thread"))
            .ok()?;
        Some(Compositor { stop, thread: Some(thread) })
    }
}

impl Drop for Compositor {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(canvas: Id, stop: &AtomicBool) {
    let gpu = match lightair_render::shared_gpu() {
        Ok(gpu) => gpu,
        Err(e) => {
            tracing::error!(%e, "compositor: no GPU; libobs draws the program");
            return;
        }
    };
    let mut state = State {
        canvas,
        name: canvas.to_string(),
        gpu,
        drawer: None,
        failed: None,
        doc: Doc::default(),
        wanted: None,
        fed: Vec::new(),
        clock: Instant::now(),
    };
    let mut next = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let period = state.tick();
        next += period;
        let now = Instant::now();
        if next <= now {
            // behind: the frames missed are dropped, not caught up
            next = now;
        } else {
            std::thread::sleep(next - now);
        }
    }
    state.idle();
    state.unfeed(&[]);
}

/// What the thread keeps between ticks.
struct State {
    canvas: Id,
    /// The canvas's id as text (how previews of it are known).
    name: String,
    gpu: SharedGpu,
    /// The renderers and buffers for the canvas's current size.
    drawer: Option<Drawer>,
    /// A size nothing could be made for (not tried again).
    failed: Option<(u32, u32)>,
    doc: Doc,
    /// The sources last asked of the bridge.
    wanted: Option<HashSet<Id>>,
    /// The previews given frames.
    fed: Vec<Weak<Shared>>,
    /// The clock moving effects run on.
    clock: Instant,
}

impl State {
    /// Draws a frame if one is wanted; the time until the next.
    fn tick(&mut self) -> Duration {
        let Some(snapshot) = bridge::snapshot() else {
            self.idle();
            return frame_period(None);
        };
        let Some(canvas) = snapshot.workspace.canvas(self.canvas).cloned() else {
            self.idle();
            return frame_period(None);
        };
        let period = frame_period(Some(&canvas));
        let primary = snapshot.workspace.canvases.first().is_some_and(|c| c.id == self.canvas);
        let (viewers, in_use) = viewers(self.canvas, &self.name, primary);
        // previews that no longer look at this canvas show libobs's frames again
        self.unfeed(&viewers);
        let showing: Vec<&Arc<Shared>> = viewers.iter().filter(|v| v.wants_program()).collect();
        if showing.is_empty() && !in_use {
            self.idle();
            return period;
        }
        let ws = self.doc.update(&snapshot.workspace);
        let now = Instant::now();
        // the switch to what's on air, while it runs
        let switch = snapshot.switch.filter(|s| ws.program == Some(s.to) && s.progress(now) < 1.0);
        let mut scenes: Vec<Id> = match switch {
            Some(s) => {
                let over = (s.settings.kind == TransitionKind::Scene)
                    .then(|| ws.scene(s.to).and_then(|sc| sc.transition_scene).or(ws.show.transition_scene))
                    .flatten();
                [Some(s.from), Some(s.to), over].into_iter().flatten().collect()
            }
            None => ws.program.into_iter().collect(),
        };
        let overlays = self.doc.overlays;
        scenes.extend(overlays);
        let size = (canvas.width.max(2) & !1, canvas.height.max(2) & !1);
        if self.drawer.as_ref().is_none_or(|d| d.size != size) {
            self.drawer = None;
            if self.failed != Some(size) {
                self.drawer = Drawer::new(&self.gpu, size);
                if self.drawer.is_none() {
                    tracing::error!(?size, "compositor: no renderer at this size; libobs draws the program");
                    self.failed = Some(size);
                }
            }
            if self.drawer.is_none() {
                self.idle();
                self.unfeed(&[]);
                return period;
            }
        }
        let shown = self.doc.shown(&ws, canvas.id, &scenes);
        if self.wanted.as_ref() != Some(&shown) {
            bridge::want(canvas.id, shown.clone());
            self.wanted = Some(shown);
        }

        let Some(drawer) = self.drawer.as_mut() else { return period };

        let time = self.clock.elapsed().as_secs_f32();
        let mut pictures = Bridged;
        match (switch, ws.program) {
            (Some(s), _) => {
                let _ = drawer.renderer.transition(
                    &ws,
                    s.from,
                    s.to,
                    canvas.id,
                    &s.settings,
                    s.progress(now),
                    &mut pictures,
                    time,
                );
            }
            // nothing on air: black, as libobs shows it
            (None, program) => {
                let _ = drawer.renderer.frame(&ws, program.unwrap_or(Id::nil()), canvas.id, &mut pictures, time);
            }
        }
        // the show's overlays, over whatever is on air
        if overlays.is_some() && drawer.over.is_none() {
            drawer.over = overlay_renderer(size);
        }
        let over = match (overlays, drawer.over.as_mut()) {
            (Some(scene), Some(r)) => {
                let _ = r.frame(&ws, scene, canvas.id, &mut pictures, time);
                Some(&*r)
            }
            _ => None,
        };
        // every buffer still in use: this frame is dropped rather than waited for
        let Some(i) = drawer.ring.free(&self.gpu.device) else { return period };
        let target = drawer.renderer.target();
        if !drawer.blit.copy(&self.gpu, target, over.map(|r| r.target()), &drawer.ring.slots[i].texture) {
            return period;
        }
        drawer.ring.handed(i);
        let frame = &drawer.ring.slots[i].buffer;
        for preview in showing {
            preview.feed(frame);
            if !self.fed.iter().any(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(preview))) {
                self.fed.push(Arc::downgrade(preview));
            }
        }
        program_source::publish(canvas.id, frame);
        period
    }

    /// Draws nothing: the bridge stops drawing sources for it, and its outputs show libobs's
    /// picture.
    fn idle(&mut self) {
        if self.wanted.take().is_some() {
            bridge::want(self.canvas, HashSet::new());
        }
        program_source::withdraw(self.canvas);
    }

    /// Previews given frames that aren't among `viewers` any more: libobs draws them again.
    fn unfeed(&mut self, viewers: &[Arc<Shared>]) {
        self.fed.retain(|w| {
            let keep = viewers.iter().any(|v| std::ptr::eq(w.as_ptr(), Arc::as_ptr(v)));
            if !keep && let Some(preview) = w.upgrade() {
                preview.unfeed();
            }
            keep
        });
    }
}

/// The canvas's frame interval (60 a second without one).
fn frame_period(canvas: Option<&Canvas>) -> Duration {
    let fps = canvas.map_or(60.0, |c| c.fps()).clamp(1.0, 240.0);
    Duration::from_secs_f64(1.0 / fps)
}

// ---------------------------------------------------------------- the document

/// The workspace as the renderer is given it, made again only when the engine's changes.
struct Doc {
    from: Option<Arc<Workspace>>,
    drawn: Option<Arc<Workspace>>,
    /// The scene the drawn workspace has for the show's overlays (every overlay scene's objects,
    /// bottom first), when there are any.
    overlays: Option<Id>,
    /// The id that scene is given.
    overlays_id: Id,
    /// The sources the last scenes asked about show: (canvas, scenes) → sources.
    shown: Option<(Id, Vec<Id>, HashSet<Id>)>,
}

impl Default for Doc {
    fn default() -> Doc {
        Doc { from: None, drawn: None, overlays: None, overlays_id: Id::new(), shown: None }
    }
}

impl Doc {
    fn update(&mut self, ws: &Arc<Workspace>) -> Arc<Workspace> {
        match (&self.from, &self.drawn) {
            (Some(from), Some(drawn)) if Arc::ptr_eq(from, ws) => drawn.clone(),
            _ => {
                let mut drawn = without_effects(ws);
                self.overlays = overlay_scene(ws, self.overlays_id).map(|scene| {
                    drawn.scenes.push(Arc::new(scene));
                    self.overlays_id
                });
                let drawn = Arc::new(drawn);
                self.from = Some(ws.clone());
                self.drawn = Some(drawn.clone());
                self.shown = None;
                drawn
            }
        }
    }

    /// The sources `scenes` show in `canvas` (visible, not see-through).
    fn shown(&mut self, ws: &Workspace, canvas: Id, scenes: &[Id]) -> HashSet<Id> {
        if let Some((c, s, shown)) = &self.shown
            && *c == canvas
            && s == scenes
        {
            return shown.clone();
        }
        let shown: HashSet<Id> = scenes
            .iter()
            .filter_map(|id| ws.scene(*id))
            .flat_map(|scene| scene.flatten().objects)
            .filter(|o| o.layouts.get(&canvas).is_some_and(|l| l.visible && l.opacity > 0.0))
            .filter_map(|o| o.source)
            .collect();
        self.shown = Some((canvas, scenes.to_vec(), shown.clone()));
        shown
    }
}

/// One scene (`id`) holding the objects of every overlay scene of the show, bottom first (a
/// scene that is also on air is drawn once more on top, as libobs does); `None` when no overlay
/// shows anything.
fn overlay_scene(ws: &Workspace, id: Id) -> Option<Scene> {
    let objects: Vec<_> = ws
        .show
        .overlays
        .iter()
        .take(OVERLAYS)
        .filter_map(|o| ws.scene(*o))
        .flat_map(|scene| scene.flatten().objects)
        .collect();
    if objects.is_empty() {
        return None;
    }
    let mut scene = Scene::new("");
    scene.id = id;
    scene.objects = objects;
    Some(scene)
}

/// `ws` with no source effects: libobs has drawn them into the sources' pictures already.
fn without_effects(ws: &Workspace) -> Workspace {
    let mut out = ws.clone();
    for source in out.sources.values_mut() {
        if !source.effects.is_empty() {
            let mut bare = Source::clone(source);
            bare.effects.clear();
            *source = Arc::new(bare);
        }
    }
    out
}

/// The sources' pictures: the bridge's newest buffers, colours already multiplied by alpha (as
/// libobs draws them).
struct Bridged;

impl Pictures for Bridged {
    fn picture(&mut self, source: &Source) -> Option<Shot> {
        let buffer = bridge::picture(source.id)?;
        let size = buffer.size();
        Some(Shot { picture: Picture::Buffer(buffer), size, premultiplied: true })
    }
}

// ---------------------------------------------------------------- drawing

/// The renderers and output buffers for one canvas size.
struct Drawer {
    size: (u32, u32),
    renderer: SceneRenderer,
    /// The show's overlays, drawn on transparent black (made when there are some).
    over: Option<SceneRenderer>,
    ring: Ring,
    blit: Blit,
}

impl Drawer {
    fn new(gpu: &SharedGpu, size: (u32, u32)) -> Option<Drawer> {
        let mut renderer =
            SceneRenderer::new(size).map_err(|e| tracing::error!(%e, "compositor: SceneRenderer")).ok()?;
        renderer.keep_on_gpu(true);
        let blit = Blit::new(&gpu.device, renderer.target().format().is_srgb());
        let mut ring = Ring { size, slots: Vec::new(), recent: VecDeque::new() };
        // one to start with: none means this device can't draw into IOSurfaces
        ring.free(&gpu.device)?;
        Some(Drawer { size, renderer, over: None, ring, blit })
    }
}

/// A renderer for the show's overlays at `size`: frames start see-through, so only what the
/// overlays cover is laid over the program.
fn overlay_renderer(size: (u32, u32)) -> Option<SceneRenderer> {
    let mut renderer =
        SceneRenderer::new(size).map_err(|e| tracing::error!(%e, "compositor: SceneRenderer for the overlays")).ok()?;
    renderer.keep_on_gpu(true);
    renderer.compositor().set_transparent(true);
    Some(renderer)
}

/// An output buffer: a BGRA IOSurface-backed pixel buffer, and a texture over its memory that
/// frames are drawn into.
struct Out {
    buffer: Frame,
    texture: wgpu::Texture,
}

impl Out {
    fn new(device: &wgpu::Device, size: (u32, u32)) -> Option<Out> {
        let (w, h) = size;
        let buffer = pixel_buffer(w, h, kCVPixelFormatType_32BGRA)?;
        let iosurface = CVPixelBufferGetIOSurface(Some(&buffer))?;
        let raw: Retained<ProtocolObject<dyn MTLTexture>> = {
            // SAFETY: the raw Metal device is only used to make a texture here.
            let hal = unsafe { device.as_hal::<wgpu::hal::api::Metal>() }?;
            // SAFETY: a plain 2D BGRA descriptor.
            let desc = unsafe {
                MTLTextureDescriptor::texture2DDescriptorWithPixelFormat_width_height_mipmapped(
                    MTLPixelFormat::BGRA8Unorm,
                    w as usize,
                    h as usize,
                    false,
                )
            };
            desc.setUsage(MTLTextureUsage::RenderTarget | MTLTextureUsage::ShaderRead);
            hal.raw_device().newTextureWithDescriptor_iosurface_plane(&desc, &iosurface, 0)?
        };
        // SAFETY: `raw` is a BGRA8Unorm 2D texture of `size` on this device; the pixel buffer that
        // owns its memory lives as long as the texture (both in `Out`).
        let hal_texture = unsafe {
            wgpu::hal::metal::Device::texture_from_raw(
                raw,
                wgpu::TextureFormat::Bgra8Unorm,
                MTLTextureType::Type2D,
                1,
                1,
                wgpu::hal::CopyExtent { width: w, height: h, depth: 1 },
                None,
            )
        };
        // SAFETY: the descriptor matches the texture made above.
        let texture = unsafe {
            device.create_texture_from_hal::<wgpu::hal::api::Metal>(
                hal_texture,
                &wgpu::TextureDescriptor {
                    label: Some("lightair-compose-out"),
                    size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format: wgpu::TextureFormat::Bgra8Unorm,
                    usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                    view_formats: &[],
                },
                wgpu::TextureUses::UNINITIALIZED,
            )
        };
        Some(Out { buffer, texture })
    }
}

/// The output buffers.
struct Ring {
    size: (u32, u32),
    slots: Vec<Out>,
    /// The ones handed on last, newest first.
    recent: VecDeque<usize>,
}

impl Ring {
    /// A buffer nothing else holds and not handed on lately; a new one while there are fewer
    /// than [`MOST`].
    fn free(&mut self, device: &wgpu::Device) -> Option<usize> {
        let free =
            (0..self.slots.len()).find(|i| !self.recent.contains(i) && self.slots[*i].buffer.retain_count() == 1);
        match free {
            Some(i) => Some(i),
            None if self.slots.len() < MOST => {
                self.slots.push(Out::new(device, self.size)?);
                Some(self.slots.len() - 1)
            }
            None => None,
        }
    }

    fn handed(&mut self, i: usize) {
        self.recent.push_front(i);
        self.recent.truncate(RECENT);
    }
}

/// Copies the renderer's frame into an output buffer, with the overlays' frame laid over it
/// when there is one (both premultiplied), sRGB-encoding it on the way when the renderers'
/// targets hold linear light.
struct Blit {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    over_pipeline: wgpu::RenderPipeline,
    over_layout: wgpu::BindGroupLayout,
}

const BLIT: &str = r#"
@group(0) @binding(0) var frame: texture_2d<f32>;
@group(0) @binding(1) var over: texture_2d<f32>;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {
    // one triangle over the whole target
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    return vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
}

fn encode(c: vec3<f32>) -> vec3<f32> {
    let lo = c * 12.92;
    let hi = 1.055 * pow(max(c, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - 0.055;
    return select(hi, lo, c <= vec3<f32>(0.0031308));
}

@fragment
fn fs_encode(@builtin(position) at: vec4<f32>) -> @location(0) vec4<f32> {
    let c = textureLoad(frame, vec2<i32>(at.xy), 0);
    return vec4<f32>(encode(c.rgb), c.a);
}

@fragment
fn fs_copy(@builtin(position) at: vec4<f32>) -> @location(0) vec4<f32> {
    return textureLoad(frame, vec2<i32>(at.xy), 0);
}

// the overlays (premultiplied) over the frame
fn overlaid(at: vec4<f32>) -> vec4<f32> {
    let p = textureLoad(frame, vec2<i32>(at.xy), 0);
    let o = textureLoad(over, vec2<i32>(at.xy), 0);
    return o + p * (1.0 - o.a);
}

@fragment
fn fs_over_encode(@builtin(position) at: vec4<f32>) -> @location(0) vec4<f32> {
    let c = overlaid(at);
    return vec4<f32>(encode(c.rgb), c.a);
}

@fragment
fn fs_over_copy(@builtin(position) at: vec4<f32>) -> @location(0) vec4<f32> {
    return overlaid(at);
}
"#;

impl Blit {
    fn new(device: &wgpu::Device, encode: bool) -> Blit {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lightair-compose-blit"),
            source: wgpu::ShaderSource::Wgsl(BLIT.into()),
        });
        let texture = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor { label: None, entries: &[texture(0)] });
        let over_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[texture(0), texture(1)],
        });
        let pipeline = |layout: &wgpu::BindGroupLayout, entry: &str| {
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: None,
                bind_group_layouts: &[Some(layout)],
                immediate_size: 0,
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("lightair-compose-blit"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(entry),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: wgpu::TextureFormat::Bgra8Unorm,
                        blend: None,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: None,
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        Blit {
            pipeline: pipeline(&layout, if encode { "fs_encode" } else { "fs_copy" }),
            over_pipeline: pipeline(&over_layout, if encode { "fs_over_encode" } else { "fs_over_copy" }),
            layout,
            over_layout,
        }
    }

    /// Draws `frame`, with `over` laid over it if given, into `out` and waits for the GPU to
    /// finish (so the buffer is complete when it's handed on). False when the GPU didn't finish
    /// in time.
    fn copy(&self, gpu: &SharedGpu, frame: &wgpu::Texture, over: Option<&wgpu::Texture>, out: &wgpu::Texture) -> bool {
        let source = frame.create_view(&Default::default());
        let over = over.map(|t| t.create_view(&Default::default()));
        let target = out.create_view(&Default::default());
        let group = match &over {
            Some(over) => gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.over_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&source) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(over) },
                ],
            }),
            None => gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: None,
                layout: &self.layout,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&source) }],
            }),
        };
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: None,
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(if over.is_some() { &self.over_pipeline } else { &self.pipeline });
            pass.set_bind_group(0, &group, &[]);
            pass.draw(0..3, 0..1);
        }
        let submitted = gpu.queue.submit([encoder.finish()]);
        gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(submitted), timeout: Some(GPU_WAIT) }).is_ok()
    }
}
