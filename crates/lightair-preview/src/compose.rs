//! macOS, with `LIGHTAIR_ENGINE=own`: the primary canvas's program drawn by Lightair's own
//! compositor (`lightair-render`'s `SceneRenderer`) instead of libobs, for a [`crate::Preview`].
//!
//! One thread at the canvas's frame rate. Each tick it reads the document the engine last
//! applied (`bridge::snapshot`), tells the bridge which sources the program shows, takes their
//! newest pictures from it (IOSurfaces libobs drew them into), and draws the frame on the GPU.
//! The frame is copied into a BGRA IOSurface from a small ring: the preview shows that buffer as
//! it shows libobs's Metal frames (the window's layer takes it as it is, GPUI's `surface()` an
//! NV12 copy), and libobs's `lightair_program` source can draw it.
//!
//! libobs draws each source through its own filters, so the renderer is given the document
//! without the sources' effects (they'd be drawn twice). It does the rest: position, fit, crop,
//! turn, flip, tilt, shape, opacity, and the switch between scenes.

use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use lightair_engine::{bridge, program_source};
use lightair_model::{Canvas, Id, Source, TransitionKind, Workspace};
use lightair_render::SharedGpu;
use lightair_render::media::Picture;
use lightair_render::scene::{Pictures, SceneRenderer, Shot};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2_core_video::{CVPixelBufferGetIOSurface, kCVPixelFormatType_32BGRA};
use objc2_metal::{MTLDevice, MTLPixelFormat, MTLTexture, MTLTextureDescriptor, MTLTextureType, MTLTextureUsage};

use crate::mac::{Frame, Shared, pixel_buffer};

/// Output buffers at most: the newest (the preview's and libobs's), the one before it (a window
/// may still be showing it), one being drawn, and spares while something holds on to older ones.
const MOST: usize = 6;
/// Buffers not drawn into again for this many frames after they're handed on (a window's layer
/// shows a surface without holding its buffer).
const RECENT: usize = 2;
/// The longest a frame's GPU work is waited for before the frame is dropped.
const GPU_WAIT: Duration = Duration::from_secs(1);

/// The compositor's thread. Dropping it stops the thread (and waits for it).
pub(crate) struct Compositor {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Compositor {
    /// Starts drawing the primary canvas's program for `preview`.
    pub(crate) fn start(preview: Arc<Shared>) -> Option<Compositor> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("lightair-compose".into())
            .spawn({
                let stop = stop.clone();
                move || {
                    run(&preview, &stop);
                    preview.unfeed();
                }
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

fn run(preview: &Shared, stop: &AtomicBool) {
    let gpu = match lightair_render::shared_gpu() {
        Ok(gpu) => gpu,
        Err(e) => {
            tracing::error!(%e, "compositor: no GPU; libobs draws the program");
            return;
        }
    };
    let mut state = State { gpu, drawer: None, failed: None, doc: Doc::default(), wanted: None, clock: Instant::now() };
    let mut next = Instant::now();
    while !stop.load(Ordering::Acquire) {
        let period = state.tick(preview);
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
}

/// What the thread keeps between ticks.
struct State {
    gpu: SharedGpu,
    /// The renderer and buffers for the canvas's current size.
    drawer: Option<Drawer>,
    /// A size nothing could be made for (not tried again).
    failed: Option<(u32, u32)>,
    doc: Doc,
    /// The canvas and sources last asked of the bridge.
    wanted: Option<(Id, HashSet<Id>)>,
    /// The clock moving effects run on.
    clock: Instant,
}

impl State {
    /// Draws a frame if one is wanted; the time until the next.
    fn tick(&mut self, preview: &Shared) -> Duration {
        let Some(snapshot) = bridge::snapshot() else {
            self.idle();
            return frame_period(None);
        };
        let Some(canvas) = snapshot.workspace.canvases.first().cloned() else {
            self.idle();
            return frame_period(None);
        };
        let period = frame_period(Some(&canvas));
        if !preview.wants_program() {
            self.idle();
            return period;
        }
        let ws = self.doc.update(&snapshot.workspace);
        let now = Instant::now();
        // the switch to what's on air, while it runs
        let switch = snapshot.switch.filter(|s| ws.program == Some(s.to) && s.progress(now) < 1.0);
        let scenes: Vec<Id> = match switch {
            Some(s) => {
                let over = (s.settings.kind == TransitionKind::Scene)
                    .then(|| ws.scene(s.to).and_then(|sc| sc.transition_scene).or(ws.show.transition_scene))
                    .flatten();
                [Some(s.from), Some(s.to), over].into_iter().flatten().collect()
            }
            None => ws.program.into_iter().collect(),
        };
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
                preview.unfeed();
                return period;
            }
        }
        let shown = self.doc.shown(&ws, canvas.id, &scenes);
        if self.wanted.as_ref().is_none_or(|(c, w)| *c != canvas.id || *w != shown) {
            if let Some((old, _)) = self.wanted.take().filter(|(c, _)| *c != canvas.id) {
                bridge::want(old, HashSet::new());
                program_source::withdraw(old);
            }
            bridge::want(canvas.id, shown.clone());
            self.wanted = Some((canvas.id, shown));
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
        // every buffer still in use: this frame is dropped rather than waited for
        let Some(i) = drawer.ring.free(&self.gpu.device) else { return period };
        if !drawer.blit.copy(&self.gpu, drawer.renderer.target(), &drawer.ring.slots[i].texture) {
            return period;
        }
        drawer.ring.handed(i);
        let frame = &drawer.ring.slots[i].buffer;
        preview.feed(frame);
        program_source::publish(canvas.id, frame);
        period
    }

    /// Draws nothing: the bridge stops drawing sources for it.
    fn idle(&mut self) {
        if let Some((canvas, _)) = self.wanted.take() {
            bridge::want(canvas, HashSet::new());
        }
    }
}

/// The canvas's frame interval (60 a second without one).
fn frame_period(canvas: Option<&Canvas>) -> Duration {
    let fps = canvas.map_or(60.0, |c| c.fps()).clamp(1.0, 240.0);
    Duration::from_secs_f64(1.0 / fps)
}

// ---------------------------------------------------------------- the document

/// The workspace as the renderer is given it, made again only when the engine's changes.
#[derive(Default)]
struct Doc {
    from: Option<Arc<Workspace>>,
    drawn: Option<Arc<Workspace>>,
    /// The sources the last scenes asked about show: (canvas, scenes) → sources.
    shown: Option<(Id, Vec<Id>, HashSet<Id>)>,
}

impl Doc {
    fn update(&mut self, ws: &Arc<Workspace>) -> Arc<Workspace> {
        match (&self.from, &self.drawn) {
            (Some(from), Some(drawn)) if Arc::ptr_eq(from, ws) => drawn.clone(),
            _ => {
                let drawn = Arc::new(without_effects(ws));
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

/// The renderer and output buffers for one canvas size.
struct Drawer {
    size: (u32, u32),
    renderer: SceneRenderer,
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
        Some(Drawer { size, renderer, ring, blit })
    }
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

/// Copies the renderer's frame into an output buffer, sRGB-encoding it on the way when the
/// renderer's target holds linear light.
struct Blit {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
}

const BLIT: &str = r#"
@group(0) @binding(0) var frame: texture_2d<f32>;

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
"#;

impl Blit {
    fn new(device: &wgpu::Device, encode: bool) -> Blit {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lightair-compose-blit"),
            source: wgpu::ShaderSource::Wgsl(BLIT.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: None,
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: None,
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
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
                entry_point: Some(if encode { "fs_encode" } else { "fs_copy" }),
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
        });
        Blit { pipeline, layout }
    }

    /// Draws `frame` into `out` and waits for the GPU to finish (so the buffer is complete when
    /// it's handed on). False when the GPU didn't finish in time.
    fn copy(&self, gpu: &SharedGpu, frame: &wgpu::Texture, out: &wgpu::Texture) -> bool {
        let source = frame.create_view(&Default::default());
        let target = out.create_view(&Default::default());
        let group = gpu.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: None,
            layout: &self.layout,
            entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&source) }],
        });
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
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &group, &[]);
            pass.draw(0..3, 0..1);
        }
        let submitted = gpu.queue.submit([encoder.finish()]);
        gpu.device.poll(wgpu::PollType::Wait { submission_index: Some(submitted), timeout: Some(GPU_WAIT) }).is_ok()
    }
}
