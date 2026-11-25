//! Avatars drawn on the GPU with wgpu, on the shared device (`lightair_render::shared_gpu`), for
//! the compositor: one [`AvatarRenderer`] per avatar source. It reads the source's settings (the
//! same keys the libobs avatar source reads), loads the model in the background, poses it each
//! frame from a face, a body, an expression and the mic ([`lightair_avatar::source`]), and draws
//! it into a texture of the source's size, see-through around the character, colours multiplied
//! by alpha. The texture is the compositor's `Picture::Texture`.
//!
//! The drawings match the libobs source's:
//!
//! - **VRM**: a toon shader close to MToon (lit and shade colours, toony and shift, rim light,
//!   matcap, emission, cutouts, unlit materials) with outlines (the inflated shell's back faces),
//!   into a texture with depth. Opaque and cut-out materials first, then blended ones far to near.
//!   Skinned on the GPU: the posed joints' matrices are uploaded each frame and every vertex is
//!   skinned in the vertex shader; the expressions' morph targets in use are added on the GPU (a
//!   compute dispatch per target) to a copy of the rest vertices first.
//! - **Live2D** and **Umamo**: each mesh in draw order with its opacity, multiply and screen
//!   colours and blend mode (normal, additive, multiply); a clipped mesh's masks drawn first into
//!   a mask texture of the frame's size, sampled by position (inverted masks too).
//! - **Inochi2D**: each part in the puppet's order with its tint, screen tint, opacity and blend
//!   mode; masks (with dodging and a threshold) into a mask texture; composites into a layer laid
//!   down after; DynamicComposites into nested layers of their own.
//! - **PNGtubers**: each visible layer a textured quad, back to front, where its pose puts it.
//!
//! All the colour maths is done on the colours as stored (sRGB-encoded, as libobs does with
//! these textures), so the result is the same picture. With `show_tracking` set, the trackers'
//! picture is drawn in the lower left corner, with the points found marked on it, when the
//! caller gives it ([`TrackingView`]).
//!
//! Threading: the model is read and put on the GPU on a loading thread; posing and drawing happen
//! on the caller's thread ([`AvatarRenderer::draw`]).

use std::collections::HashMap;
use std::num::NonZeroU64;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::task::{Context, Poll, Waker};
use std::time::Instant;

use glam::{Mat4, Vec3};
use lightair_avatar::body::BodyFrame;
use lightair_avatar::inochi2d::{self, Look, Op, TextureFrame};
use lightair_avatar::lightair_moc3::moc3::Moc3DrawableBlendMode;
use lightair_avatar::source::{self, Flat, Input, Pngtuber, Posable, Poser, Settings};
use lightair_avatar::vrm::{AlphaMode, Material, mip_chain};
use lightair_avatar::{Avatar, live2d, uma};
use lightair_render::media::Picture;
use lightair_render::scene::Shot;
use wgpu::util::DeviceExt;

/// What an avatar's model is doing, for the Inspector.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    /// The model is read and on the GPU.
    pub loaded: bool,
    /// The model is being read.
    pub loading: bool,
    /// Why the model didn't load.
    pub error: String,
    /// The model's own name, or its file's.
    pub name: String,
    /// Its expressions (a VRM's moods and its own; a Live2D model's; the parameters an Inochi2D
    /// or Umamo puppet's face doesn't drive; a PNGtuber's).
    pub expressions: Vec<String>,
    /// Which of its expressions are costumes (a PNGtuber's).
    pub costumes: Vec<String>,
}

/// The trackers' picture and what they found in it, drawn in the avatar's lower left corner
/// when its `show_tracking` setting is on.
pub struct TrackingView<'a> {
    /// BGRA, rows `width * 4` bytes apart, top row first.
    pub bgra: &'a [u8],
    pub width: u32,
    pub height: u32,
    /// The face's points, 0..1 across and down the picture.
    pub face: &'a [[f32; 2]],
    /// The body's joints and hands.
    pub body: Option<&'a BodyFrame>,
}

/// An avatar source drawn on the GPU.
pub struct AvatarRenderer {
    device: wgpu::Device,
    queue: wgpu::Queue,
    settings: Settings,
    /// The file and revision last asked for.
    asked: Option<(String, i64)>,
    /// The model being read.
    pending: Option<Receiver<Result<Box<Ready>, String>>>,
    ready: Option<Box<Ready>>,
    status: Status,
    poser: Poser,
    targets: Option<Targets>,
    scratch: Option<Scratch>,
    last: Option<Instant>,
}

/// A model read and on the GPU.
struct Ready {
    model: Posable,
    gpu: ModelGpu,
    name: String,
    expressions: Vec<String>,
    costumes: Vec<String>,
}

enum ModelGpu {
    Vrm(Box<VrmGpu>),
    Flat(Box<FlatGpu>),
}

impl AvatarRenderer {
    /// A renderer for an avatar source with these settings (its JSON object); its model starts
    /// loading now. `Err` without a GPU.
    pub fn new(settings: &serde_json::Value) -> Result<AvatarRenderer, String> {
        let gpu = lightair_render::shared_gpu()?;
        let mut r = AvatarRenderer {
            device: gpu.device,
            queue: gpu.queue,
            settings: Settings::default(),
            asked: None,
            pending: None,
            ready: None,
            status: Status::default(),
            poser: Poser::default(),
            targets: None,
            scratch: None,
            last: None,
        };
        r.set_settings(settings);
        Ok(r)
    }

    /// The source's settings changed: a new file (or the same one saved again) is read in the
    /// background, and nothing is drawn until it's ready.
    pub fn set_settings(&mut self, settings: &serde_json::Value) {
        self.settings = Settings::from_json(settings);
        let wanted = (self.settings.file.clone(), self.settings.revision);
        if self.asked.as_ref() != Some(&wanted) {
            self.asked = Some(wanted);
            self.start_load();
        }
    }

    /// The settings in force.
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The picture's size (the source's `width` and `height`).
    pub fn size(&self) -> (u32, u32) {
        (self.settings.width, self.settings.height)
    }

    /// Whether the model is ready, why it isn't, its name and expressions.
    pub fn status(&mut self) -> Status {
        self.poll();
        self.status.clone()
    }

    /// Poses the avatar for this frame and draws it, submitting the work to the shared queue.
    /// `None` while there's no model to draw (none chosen, loading, failed): nothing shows.
    pub fn draw(&mut self, input: &Input, view: Option<&TrackingView>) -> Option<Shot> {
        let scope = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let mut encoder =
            self.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("lightair-avatar") });
        let shot = self.encode(&mut encoder, input, view);
        if shot.is_some() {
            self.queue.submit([encoder.finish()]);
        }
        if let Some(e) = popped(scope) {
            tracing::warn!(error = %e, "avatar: drawing failed");
            return None;
        }
        shot
    }

    /// Draws the avatar at rest, nothing tracked (a still picture of it).
    pub fn draw_at_rest(&mut self) -> Option<Shot> {
        self.draw(&Input::default(), None)
    }

    /// As [`AvatarRenderer::draw`], recording into `encoder` (the uploads it needs are queued
    /// and land at the caller's next submit, before the encoder's work).
    pub fn encode(
        &mut self,
        encoder: &mut wgpu::CommandEncoder,
        input: &Input,
        view: Option<&TrackingView>,
    ) -> Option<Shot> {
        self.poll();
        let now = Instant::now();
        let dt = match self.last.map(|t| now.duration_since(t).as_secs_f32()) {
            // (shown again after a while: as a first frame)
            Some(dt) if dt <= 1.0 => dt,
            _ => 1.0 / 60.0,
        };
        self.last = Some(now);
        let dt = if check_mode().0 { 0.0 } else { dt };
        let ready = self.ready.as_mut()?;
        self.poser.step(&mut ready.model, &self.settings, input, dt);
        let device = &self.device;
        let pipes = pipes(device, &self.queue);
        let size = (self.settings.width, self.settings.height);
        let targets = match &mut self.targets {
            Some(t) if t.size == size => t,
            slot => slot.insert(Targets::new(device, pipes, size)),
        };
        targets.next ^= 1;
        let scratch = self.scratch.get_or_insert_with(|| Scratch::new(device, pipes));
        let mut plan = Plan::default();
        let mut flat = None;
        match (&mut ready.model, &mut ready.gpu) {
            (Posable::Vrm(a), ModelGpu::Vrm(g)) => {
                let depth = targets.depth(device);
                let out = &targets.out[targets.next].view;
                g.encode(&self.queue, pipes, encoder, a, &self.settings, out, &depth);
            }
            (Posable::Flat(f), ModelGpu::Flat(g)) => {
                g.upload(&self.queue, f);
                plan.target(Tgt::Out, Some(0.0));
                plan_flat(&mut plan, f, g, &self.settings);
                flat = Some(&**g);
            }
            _ => return None,
        }
        let view_group = match (view, self.settings.show_tracking) {
            (Some(v), true) => scratch.view(device, &self.queue, pipes, v, self.settings.mirror),
            _ => None,
        };
        if view_group.is_some() {
            plan_view(&mut plan, &self.settings, scratch.view_size);
        }
        if !plan.steps.is_empty() {
            for t in plan.targets() {
                targets.ensure(device, pipes, t);
            }
            scratch.prepare(device, &self.queue, pipes, &plan);
            run(device, encoder, &plan, pipes, scratch, targets, flat, view_group.as_ref());
        }
        let texture = targets.out[targets.next].texture.clone();
        Some(Shot { picture: Picture::Texture(texture), size, premultiplied: true })
    }

    fn start_load(&mut self) {
        self.ready = None;
        self.pending = None;
        let file = self.settings.file.clone();
        if file.is_empty() {
            self.status = Status::default();
            return;
        }
        self.status = Status { loading: true, ..Status::default() };
        let (tx, rx) = std::sync::mpsc::channel();
        let (device, queue) = (self.device.clone(), self.queue.clone());
        let spawned = std::thread::Builder::new().name("lightair-avatar-load".into()).spawn(move || {
            let result = load(&device, &queue, &file);
            match &result {
                Ok(_) => tracing::info!(file, "avatar loaded (GPU)"),
                Err(e) => tracing::warn!(file, error = %e, "avatar didn't load (GPU)"),
            }
            // (nobody waits for it any more when another file was chosen meanwhile)
            let _ = tx.send(result);
        });
        match spawned {
            Ok(_) => self.pending = Some(rx),
            Err(e) => self.status = Status { error: e.to_string(), ..Status::default() },
        }
    }

    /// Takes the model the loading thread finished.
    fn poll(&mut self) {
        let Some(rx) = &self.pending else { return };
        let result = match rx.try_recv() {
            Ok(r) => r,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => Err("the model's loading stopped".into()),
        };
        self.pending = None;
        match result {
            Ok(ready) => {
                self.status = Status {
                    loaded: true,
                    loading: false,
                    error: String::new(),
                    name: ready.name.clone(),
                    expressions: ready.expressions.clone(),
                    costumes: ready.costumes.clone(),
                };
                self.ready = Some(ready);
            }
            Err(e) => self.status = Status { error: e, ..Status::default() },
        }
    }
}

/// Reads an avatar's file and puts it on the GPU (on the loading thread).
fn load(device: &wgpu::Device, queue: &wgpu::Queue, file: &str) -> Result<Box<Ready>, String> {
    let loaded = source::load(file)?;
    let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let pipes = pipes(device, queue);
    let mut model = loaded.model;
    let gpu = match &mut model {
        Posable::Vrm(a) => {
            a.set_cpu_skinning(false);
            ModelGpu::Vrm(Box::new(VrmGpu::new(device, queue, pipes, a)))
        }
        Posable::Flat(f) => {
            let g = FlatGpu::new(device, queue, pipes, f);
            g.prepare_pipelines(device, pipes, f);
            ModelGpu::Flat(Box::new(g))
        }
    };
    if let Some(e) = popped(scope) {
        return Err(format!("the model couldn't be put on the GPU: {e}"));
    }
    Ok(Box::new(Ready { model, gpu, name: loaded.name, expressions: loaded.expressions, costumes: loaded.costumes }))
}

/// The error an error scope caught, if any (wgpu's native scopes answer at once).
fn popped(scope: wgpu::ErrorScopeGuard) -> Option<wgpu::Error> {
    let mut future = std::pin::pin!(scope.pop());
    match future.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(e) => e,
        Poll::Pending => None,
    }
}

/// Development checks (`LIGHTAIR_AVATAR_CHECK`, as the libobs source reads it): `still` holds
/// the pose (no time passes, for pictures that can be compared), `still-unmerged` also draws a
/// VRM's primitives each on its own.
fn check_mode() -> (bool, bool) {
    static MODE: OnceLock<(bool, bool)> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("LIGHTAIR_AVATAR_CHECK").as_deref() {
        Ok("still") => (true, false),
        Ok("still-unmerged") => (true, true),
        _ => (false, false),
    })
}

// ---------------------------------------------------------------- shared GPU objects

/// The format every avatar picture and layer is drawn in: colours as stored, not decoded.
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
const DEPTH: wgpu::TextureFormat = wgpu::TextureFormat::Depth32Float;
/// DynamicComposites nested this deep get a layer (deeper ones draw nothing), as in libobs.
const DYNAMIC_LEVELS: usize = 4;

/// The shaders and pipelines every avatar shares (one shared device per process).
struct Pipes {
    /// VRM: the frame's group (uniforms, joint palette, morphed vertices), a material's group.
    vrm_frame: wgpu::BindGroupLayout,
    vrm_material: wgpu::BindGroupLayout,
    /// Drawn with back faces culled, with none culled (double-sided), and outlines (front
    /// faces culled).
    vrm: [wgpu::RenderPipeline; 3],
    morph_layout: wgpu::BindGroupLayout,
    morph: wgpu::ComputePipeline,
    /// Wraps (VRM textures).
    repeat: wgpu::Sampler,
    /// 2D: a draw's uniforms (dynamic offset) and the sampler; a texture.
    flat_draw: wgpu::BindGroupLayout,
    texture: wgpu::BindGroupLayout,
    flat_module: wgpu::ShaderModule,
    flat_layout: wgpu::PipelineLayout,
    flat: Mutex<HashMap<PipeKey, wgpu::RenderPipeline>>,
    /// Clamps (2D textures, masks, layers).
    clamp: wgpu::Sampler,
    /// 1×1 opaque white: a missing texture, an unused mask.
    white: wgpu::TextureView,
    white_group: wgpu::BindGroup,
    uniform_align: u64,
}

static PIPES: OnceLock<Pipes> = OnceLock::new();

fn pipes(device: &wgpu::Device, queue: &wgpu::Queue) -> &'static Pipes {
    PIPES.get_or_init(|| Pipes::new(device, queue))
}

fn tex_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn buffer_entry(
    binding: u32,
    visibility: wgpu::ShaderStages,
    ty: wgpu::BufferBindingType,
    dynamic: bool,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer { ty, has_dynamic_offset: dynamic, min_binding_size: None },
        count: None,
    }
}

fn sampler_entry(binding: u32, visibility: wgpu::ShaderStages) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

impl Pipes {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue) -> Pipes {
        let uniform_align = u64::from(device.limits().min_uniform_buffer_offset_alignment.max(16));
        let sampler = |address| {
            device.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("lightair-avatar"),
                address_mode_u: address,
                address_mode_v: address,
                address_mode_w: address,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::MipmapFilterMode::Linear,
                ..Default::default()
            })
        };
        let repeat = sampler(wgpu::AddressMode::Repeat);
        let clamp = sampler(wgpu::AddressMode::ClampToEdge);
        let both = wgpu::ShaderStages::VERTEX_FRAGMENT;
        let frag = wgpu::ShaderStages::FRAGMENT;
        let read = wgpu::BufferBindingType::Storage { read_only: true };

        // VRM
        let vrm_frame = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lightair-avatar-vrm-frame"),
            entries: &[
                buffer_entry(0, both, wgpu::BufferBindingType::Uniform, false),
                buffer_entry(1, wgpu::ShaderStages::VERTEX, read, false),
                buffer_entry(2, wgpu::ShaderStages::VERTEX, read, false),
            ],
        });
        let vrm_material = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lightair-avatar-vrm-material"),
            entries: &[
                buffer_entry(0, both, wgpu::BufferBindingType::Uniform, false),
                tex_entry(1, frag),
                tex_entry(2, frag),
                tex_entry(3, frag),
                tex_entry(4, frag),
                sampler_entry(5, frag),
            ],
        });
        let vrm_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lightair-avatar-vrm"),
            source: wgpu::ShaderSource::Wgsl(VRM_SHADER.into()),
        });
        let vrm_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("lightair-avatar-vrm"),
            bind_group_layouts: &[Some(&vrm_frame), Some(&vrm_material)],
            immediate_size: 0,
        });
        let vrm_attributes = [
            wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 0, shader_location: 0 },
            wgpu::VertexAttribute { format: wgpu::VertexFormat::Uint32x4, offset: 8, shader_location: 1 },
            wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x4, offset: 24, shader_location: 2 },
        ];
        // colour over what's drawn, alpha added up: the picture comes out premultiplied
        let vrm_blend = wgpu::BlendState {
            color: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::SrcAlpha,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
            alpha: wgpu::BlendComponent {
                src_factor: wgpu::BlendFactor::One,
                dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                operation: wgpu::BlendOperation::Add,
            },
        };
        let vrm_pipeline = |vs: &str, fs: &str, cull: Option<wgpu::Face>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("lightair-avatar-vrm"),
                layout: Some(&vrm_layout),
                vertex: wgpu::VertexState {
                    module: &vrm_module,
                    entry_point: Some(vs),
                    buffers: &[Some(wgpu::VertexBufferLayout {
                        array_stride: VRM_STRIDE,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &vrm_attributes,
                    })],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &vrm_module,
                    entry_point: Some(fs),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: FORMAT,
                        blend: Some(vrm_blend),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::TriangleList,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: cull,
                    ..Default::default()
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: DEPTH,
                    depth_write_enabled: Some(true),
                    depth_compare: Some(wgpu::CompareFunction::Less),
                    stencil: Default::default(),
                    bias: Default::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let vrm = [
            vrm_pipeline("vs_main", "fs_main", Some(wgpu::Face::Back)),
            vrm_pipeline("vs_main", "fs_main", None),
            vrm_pipeline("vs_outline", "fs_outline", Some(wgpu::Face::Front)),
        ];
        let compute = wgpu::ShaderStages::COMPUTE;
        let morph_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lightair-avatar-morph"),
            entries: &[
                buffer_entry(0, compute, read, false),
                buffer_entry(1, compute, wgpu::BufferBindingType::Storage { read_only: false }, false),
                buffer_entry(2, compute, wgpu::BufferBindingType::Uniform, true),
            ],
        });
        let morph_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lightair-avatar-morph"),
            source: wgpu::ShaderSource::Wgsl(MORPH_SHADER.into()),
        });
        let morph_pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("lightair-avatar-morph"),
            bind_group_layouts: &[Some(&morph_layout)],
            immediate_size: 0,
        });
        let morph = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("lightair-avatar-morph"),
            layout: Some(&morph_pipeline_layout),
            module: &morph_module,
            entry_point: Some("morph"),
            compilation_options: Default::default(),
            cache: None,
        });

        // 2D
        let flat_draw = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lightair-avatar-2d"),
            entries: &[buffer_entry(0, both, wgpu::BufferBindingType::Uniform, true), sampler_entry(1, frag)],
        });
        let texture = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("lightair-avatar-texture"),
            entries: &[tex_entry(0, frag)],
        });
        let flat_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("lightair-avatar-2d"),
            source: wgpu::ShaderSource::Wgsl(FLAT_SHADER.into()),
        });
        let flat_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("lightair-avatar-2d"),
            bind_group_layouts: &[Some(&flat_draw), Some(&texture), Some(&texture)],
            immediate_size: 0,
        });
        let white = device.create_texture_with_data(
            queue,
            &wgpu::TextureDescriptor {
                label: Some("lightair-avatar-white"),
                size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            },
            wgpu::util::TextureDataOrder::LayerMajor,
            &[255; 4],
        );
        let white = white.create_view(&Default::default());
        let white_group = texture_group(device, &texture, &white);
        Pipes {
            vrm_frame,
            vrm_material,
            vrm,
            morph_layout,
            morph,
            repeat,
            flat_draw,
            texture,
            flat_module,
            flat_layout,
            flat: Mutex::new(HashMap::new()),
            clamp,
            white,
            white_group,
            uniform_align,
        }
    }

    /// A 2D pipeline, made the first time it's asked for.
    fn flat(&self, device: &wgpu::Device, key: PipeKey) -> wgpu::RenderPipeline {
        let mut cache = self.flat.lock().unwrap_or_else(|e| e.into_inner());
        cache
            .entry(key)
            .or_insert_with(|| {
                let attribute = |location| {
                    [wgpu::VertexAttribute {
                        format: wgpu::VertexFormat::Float32x2,
                        offset: 0,
                        shader_location: location,
                    }]
                };
                let (position, uv) = (attribute(0), attribute(1));
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("lightair-avatar-2d"),
                    layout: Some(&self.flat_layout),
                    vertex: wgpu::VertexState {
                        module: &self.flat_module,
                        entry_point: Some(key.vs.entry()),
                        buffers: &[
                            Some(wgpu::VertexBufferLayout {
                                array_stride: 8,
                                step_mode: wgpu::VertexStepMode::Vertex,
                                attributes: &position,
                            }),
                            Some(wgpu::VertexBufferLayout {
                                array_stride: 8,
                                step_mode: wgpu::VertexStepMode::Vertex,
                                attributes: &uv,
                            }),
                        ],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &self.flat_module,
                        entry_point: Some(key.fs.entry()),
                        targets: &[Some(wgpu::ColorTargetState {
                            format: FORMAT,
                            blend: Some(key.blend.state()),
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                        compilation_options: Default::default(),
                    }),
                    primitive: wgpu::PrimitiveState {
                        topology: wgpu::PrimitiveTopology::TriangleList,
                        cull_mode: None,
                        ..Default::default()
                    },
                    depth_stencil: None,
                    multisample: wgpu::MultisampleState::default(),
                    multiview_mask: None,
                    cache: None,
                })
            })
            .clone()
    }
}

/// A bind group showing one texture (the 2D shaders' image or mask).
fn texture_group(device: &wgpu::Device, layout: &wgpu::BindGroupLayout, view: &wgpu::TextureView) -> wgpu::BindGroup {
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("lightair-avatar-texture"),
        layout,
        entries: &[wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(view) }],
    })
}

/// An RGBA8 picture as a texture, with its smaller copies (`mips`, each half the last) as its
/// mip levels. `None` when the picture is empty, too big for the GPU or short of pixels.
fn upload_texture(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    width: u32,
    height: u32,
    rgba: &[u8],
    mips: &[Vec<u8>],
) -> Option<wgpu::Texture> {
    let max = device.limits().max_texture_dimension_2d;
    if width == 0 || height == 0 || width > max || height > max || rgba.len() < width as usize * height as usize * 4 {
        return None;
    }
    let mut levels: Vec<(&[u8], u32, u32)> = vec![(rgba, width, height)];
    let (mut w, mut h) = (width, height);
    for m in mips {
        (w, h) = ((w / 2).max(1), (h / 2).max(1));
        if m.len() < w as usize * h as usize * 4 {
            break;
        }
        levels.push((m, w, h));
    }
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("lightair-avatar"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: levels.len() as u32,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for (level, (data, w, h)) in levels.into_iter().enumerate() {
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: level as u32,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &data[..w as usize * h as usize * 4],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(4 * w), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
    }
    Some(texture)
}

/// A buffer holding `contents` (at least `min` bytes, so nothing binds an empty buffer).
fn buffer(device: &wgpu::Device, label: &str, contents: &[u8], min: usize, usage: wgpu::BufferUsages) -> wgpu::Buffer {
    if contents.len() >= min {
        device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some(label), contents, usage })
    } else {
        let mut padded = contents.to_vec();
        padded.resize(min, 0);
        device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some(label), contents: &padded, usage })
    }
}

// ---------------------------------------------------------------- VRM

/// A VRM vertex's static attributes: texture coordinate, four joints (into the palette), their
/// weights.
const VRM_STRIDE: u64 = 40;
/// A vertex as the shaders read it: position and normal, each four floats.
const VERTEX_BYTES: u64 = 32;

/// One draw: the primitives of a material merged into one range of indices (a blended
/// material's primitives each on their own, as they're sorted by depth every frame), as the
/// libobs source draws them.
struct Draw3 {
    first: u32,
    count: u32,
    /// Into [`VrmGpu::materials`] (the last: a missing material's defaults).
    material: usize,
    blended: bool,
    queue_offset: i32,
    outline: bool,
    double_sided: bool,
    /// A few of a blended draw's vertices at rest, for its depth (each with its joints and
    /// weights).
    samples: Vec<(Vec3, [u32; 4], [f32; 4])>,
}

/// A primitive's vertices in the shared buffers and its morph targets' deltas.
struct PrimGpu {
    mesh: usize,
    at: u32,
    len: u32,
    /// Each target's deltas: where they start in the deltas buffer, how many.
    targets: Vec<(u32, u32)>,
    /// Morphed last frame: its vertices must be put back to rest first.
    morphed: bool,
}

/// A VRM model on the GPU.
struct VrmGpu {
    statics: wgpu::Buffer,
    base: wgpu::Buffer,
    verts: wgpu::Buffer,
    indices: wgpu::Buffer,
    palette: wgpu::Buffer,
    frame: wgpu::Buffer,
    jobs: wgpu::Buffer,
    job_capacity: usize,
    frame_group: wgpu::BindGroup,
    morph_group: wgpu::BindGroup,
    materials: Vec<wgpu::BindGroup>,
    draws: Vec<Draw3>,
    prims: Vec<PrimGpu>,
    /// The palette: each skin's joints (skin, joint count), then the nodes of the primitives
    /// without a skin.
    skins: Vec<(usize, usize)>,
    nodes: Vec<usize>,
}

impl VrmGpu {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue, pipes: &Pipes, a: &Avatar) -> VrmGpu {
        let model = &a.model;
        // the palette's layout
        let mut skins = Vec::new();
        let mut skin_base = Vec::new();
        let mut at = 0u32;
        for (s, skin) in model.skins.iter().enumerate() {
            skin_base.push(at);
            skins.push((s, skin.joints.len()));
            at += skin.joints.len() as u32;
        }
        let mut nodes: Vec<usize> = Vec::new();
        let mut node_slot: HashMap<usize, u32> = HashMap::new();
        for p in &model.primitives {
            if p.skin.and_then(|s| model.skins.get(s)).is_none() && !node_slot.contains_key(&p.node) {
                node_slot.insert(p.node, at + nodes.len() as u32);
                nodes.push(p.node);
            }
        }
        let palette_len = at as usize + nodes.len();

        // the primitives grouped by material, in the order the materials first come
        let unmerged = check_mode().1;
        let blended_material = |m: usize| model.materials.get(m).is_some_and(|m| m.alpha == AlphaMode::Blend);
        let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
        for (i, p) in model.primitives.iter().enumerate() {
            let blended = blended_material(p.material);
            match groups.iter_mut().find(|(m, _)| *m == p.material && !blended && !unmerged) {
                Some((_, list)) => list.push(i),
                None => groups.push((p.material, vec![i])),
            }
        }

        let mut statics: Vec<u32> = Vec::new();
        let mut base: Vec<f32> = Vec::new();
        let mut indices: Vec<u32> = Vec::new();
        let mut deltas: Vec<u32> = Vec::new();
        let mut prims: Vec<Option<PrimGpu>> = (0..model.primitives.len()).map(|_| None).collect();
        let mut draws = Vec::new();
        let mut vertex_at = 0u32;
        let default_material = Material::default();
        for (material, list) in groups {
            let first = indices.len() as u32;
            let mut samples = Vec::new();
            for &i in &list {
                let p = &model.primitives[i];
                let k = p.positions.len();
                let skin = p.skin.and_then(|s| model.skins.get(s).map(|skin| (skin_base[s], skin.joints.len())));
                let attributes = |v: usize| -> ([u32; 4], [f32; 4]) {
                    match skin {
                        Some((offset, count)) => {
                            let js = p.joints.get(v).copied().unwrap_or([0; 4]);
                            let mut ws = p.weights.get(v).copied().unwrap_or([0.0; 4]);
                            let mut joints = [0u32; 4];
                            for c in 0..4 {
                                if (js[c] as usize) < count {
                                    joints[c] = offset + u32::from(js[c]);
                                } else {
                                    // (a joint the skin doesn't have counts for nothing)
                                    ws[c] = 0.0;
                                }
                            }
                            (joints, ws)
                        }
                        None => ([node_slot.get(&p.node).copied().unwrap_or(0), 0, 0, 0], [1.0, 0.0, 0.0, 0.0]),
                    }
                };
                for v in 0..k {
                    let uv = p.uvs.get(v).copied().unwrap_or([0.0; 2]);
                    let (joints, weights) = attributes(v);
                    statics.extend([uv[0].to_bits(), uv[1].to_bits()]);
                    statics.extend(joints);
                    statics.extend(weights.map(f32::to_bits));
                    let pos = p.positions[v];
                    let n = p.normals.get(v).copied().unwrap_or(Vec3::Z);
                    base.extend([pos.x, pos.y, pos.z, 1.0, n.x, n.y, n.z, 0.0]);
                }
                indices.extend(p.indices.iter().map(|&x| x + vertex_at));
                if blended_material(material) && samples.is_empty() {
                    samples = (0..k)
                        .step_by((k / 16).max(1))
                        .map(|v| {
                            let (j, w) = attributes(v);
                            (p.positions[v], j, w)
                        })
                        .collect();
                }
                // each target's deltas, positions and normals together, one entry a vertex
                let mut targets = Vec::new();
                for m in &p.morphs {
                    let mut merged: std::collections::BTreeMap<u32, (Vec3, Vec3)> = Default::default();
                    for &(v, d) in &m.positions {
                        merged.entry(v).or_default().0 += d;
                    }
                    for &(v, d) in &m.normals {
                        merged.entry(v).or_default().1 += d;
                    }
                    let start = (deltas.len() / 8) as u32;
                    let mut count = 0u32;
                    for (v, (dp, dn)) in merged {
                        if (v as usize) < k {
                            deltas.extend([dp.x.to_bits(), dp.y.to_bits(), dp.z.to_bits(), vertex_at + v]);
                            deltas.extend([dn.x.to_bits(), dn.y.to_bits(), dn.z.to_bits(), 0]);
                            count += 1;
                        }
                    }
                    targets.push((start, count));
                }
                prims[i] = Some(PrimGpu { mesh: p.mesh, at: vertex_at, len: k as u32, targets, morphed: false });
                vertex_at += k as u32;
            }
            let mat = model.materials.get(material).unwrap_or(&default_material);
            draws.push(Draw3 {
                first,
                count: indices.len() as u32 - first,
                material: if material < model.materials.len() { material } else { model.materials.len() },
                blended: mat.alpha == AlphaMode::Blend,
                queue_offset: mat.queue_offset,
                outline: mat.outline_width > 0.0 && mat.alpha != AlphaMode::Blend,
                double_sided: mat.double_sided,
                samples,
            });
        }
        let prims: Vec<PrimGpu> = prims.into_iter().flatten().collect();
        let job_capacity = prims.iter().map(|p| p.targets.len()).sum::<usize>().max(1);

        let storage = wgpu::BufferUsages::STORAGE;
        let statics = buffer(
            device,
            "lightair-avatar-vrm-statics",
            bytemuck::cast_slice(&statics),
            64,
            wgpu::BufferUsages::VERTEX,
        );
        let base_bytes: &[u8] = bytemuck::cast_slice(&base);
        let base = buffer(device, "lightair-avatar-vrm-base", base_bytes, 64, storage | wgpu::BufferUsages::COPY_SRC);
        let verts =
            buffer(device, "lightair-avatar-vrm-vertices", base_bytes, 64, storage | wgpu::BufferUsages::COPY_DST);
        let indices = buffer(
            device,
            "lightair-avatar-vrm-indices",
            bytemuck::cast_slice(&indices),
            16,
            wgpu::BufferUsages::INDEX,
        );
        let deltas = buffer(device, "lightair-avatar-vrm-deltas", bytemuck::cast_slice(&deltas), 64, storage);
        let palette = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lightair-avatar-vrm-palette"),
            size: (palette_len.max(1) * 64) as u64,
            usage: storage | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let frame = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lightair-avatar-vrm-frame"),
            size: FRAME_BYTES,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let jobs = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lightair-avatar-vrm-morph-jobs"),
            size: job_capacity as u64 * pipes.uniform_align,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let frame_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("lightair-avatar-vrm-frame"),
            layout: &pipes.vrm_frame,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: frame.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: palette.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 2, resource: verts.as_entire_binding() },
            ],
        });
        let morph_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("lightair-avatar-vrm-morph"),
            layout: &pipes.morph_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: deltas.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: verts.as_entire_binding() },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &jobs,
                        offset: 0,
                        size: NonZeroU64::new(16),
                    }),
                },
            ],
        });

        // the textures, then a group per material (and one with the defaults)
        let views: Vec<Option<wgpu::TextureView>> = model
            .images
            .iter()
            .map(|img| {
                upload_texture(device, queue, img.width, img.height, &img.rgba, &img.mips)
                    .map(|t| t.create_view(&Default::default()))
            })
            .collect();
        let tex = |t: Option<usize>| t.and_then(|t| views.get(t)).and_then(Option::as_ref).unwrap_or(&pipes.white);
        let materials = model
            .materials
            .iter()
            .chain(std::iter::once(&default_material))
            .map(|mat| {
                let uniform = buffer(
                    device,
                    "lightair-avatar-vrm-material",
                    bytemuck::cast_slice(&material_uniform(mat)),
                    0,
                    wgpu::BufferUsages::UNIFORM,
                );
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("lightair-avatar-vrm-material"),
                    layout: &pipes.vrm_material,
                    entries: &[
                        wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(tex(mat.base_texture)),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::TextureView(tex(mat.shade_texture)),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::TextureView(tex(mat.emissive_texture)),
                        },
                        wgpu::BindGroupEntry {
                            binding: 4,
                            resource: wgpu::BindingResource::TextureView(tex(mat.matcap_texture)),
                        },
                        wgpu::BindGroupEntry { binding: 5, resource: wgpu::BindingResource::Sampler(&pipes.repeat) },
                    ],
                })
            })
            .collect();
        let outlines = draws.iter().filter(|d| d.count > 0 && d.outline).count();
        tracing::info!(
            primitives = model.primitives.len(),
            draws = draws.iter().filter(|d| d.count > 0).count(),
            outlines,
            vertices = vertex_at,
            "VRM on the GPU (wgpu)"
        );
        VrmGpu {
            statics,
            base,
            verts,
            indices,
            palette,
            frame,
            jobs,
            job_capacity,
            frame_group,
            morph_group,
            materials,
            draws,
            prims,
            skins,
            nodes,
        }
    }

    /// Skins, morphs and draws the posed avatar into `out` (cleared first).
    #[allow(clippy::too_many_arguments)]
    fn encode(
        &mut self,
        queue: &wgpu::Queue,
        pipes: &Pipes,
        encoder: &mut wgpu::CommandEncoder,
        a: &Avatar,
        settings: &Settings,
        out: &wgpu::TextureView,
        depth: &wgpu::TextureView,
    ) {
        // the joints' matrices this frame
        let mut palette: Vec<Mat4> = Vec::new();
        for &(skin, count) in &self.skins {
            let m = a.joint_matrices(skin);
            palette.extend((0..count).map(|k| m.get(k).copied().unwrap_or(Mat4::IDENTITY)));
        }
        palette.extend(self.nodes.iter().map(|&n| a.world.get(n).copied().unwrap_or(Mat4::IDENTITY)));
        if !palette.is_empty() {
            let floats: Vec<f32> = palette.iter().flat_map(|m| m.to_cols_array()).collect();
            queue.write_buffer(&self.palette, 0, bytemuck::cast_slice(&floats));
        }

        // where it's seen from
        let (w, h) = (settings.width as f32, settings.height as f32);
        let aspect = w / h;
        let cam = a.camera(settings.framing, aspect, settings.zoom);
        let view = cam.view();
        let proj = Mat4::perspective_rh(cam.fovy.to_radians(), aspect, cam.near, cam.far);
        let light = Vec3::new(0.35, 0.6, 1.0).normalize();
        let mut frame: Vec<f32> = Vec::with_capacity(40);
        frame.extend((proj * view).to_cols_array());
        frame.extend(view.to_cols_array());
        frame.extend([light.x, light.y, light.z, 0.0, cam.eye.x, cam.eye.y, cam.eye.z, 1.0]);
        queue.write_buffer(&self.frame, 0, bytemuck::cast_slice(&frame));

        // the expressions' morph targets in use, added to the rest vertices
        let align = pipes.uniform_align as usize;
        let mut jobs: Vec<(u32, u32, f32)> = Vec::new();
        let mut resets: Vec<(u32, u32)> = Vec::new();
        for p in &mut self.prims {
            let weights = a.morph_weights(p.mesh).unwrap_or(&[]);
            let before = jobs.len();
            for (t, &(start, count)) in p.targets.iter().enumerate() {
                let wt = weights.get(t).copied().unwrap_or(0.0);
                if wt > 1e-4 && count > 0 {
                    jobs.push((start, count, wt));
                }
            }
            let morphed = jobs.len() > before;
            if (p.morphed || morphed) && p.len > 0 {
                resets.push((p.at, p.len));
            }
            p.morphed = morphed;
        }
        jobs.truncate(self.job_capacity);
        for &(at, len) in &resets {
            let (offset, size) = (u64::from(at) * VERTEX_BYTES, u64::from(len) * VERTEX_BYTES);
            encoder.copy_buffer_to_buffer(&self.base, offset, &self.verts, offset, Some(size));
        }
        if !jobs.is_empty() {
            let mut bytes = vec![0u8; jobs.len() * align];
            for (k, &(start, count, wt)) in jobs.iter().enumerate() {
                let job: [u32; 4] = [start, count, wt.to_bits(), 0];
                bytes[k * align..k * align + 16].copy_from_slice(bytemuck::cast_slice(&job));
            }
            queue.write_buffer(&self.jobs, 0, &bytes);
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("lightair-avatar-morph"),
                timestamp_writes: None,
            });
            pass.set_pipeline(&pipes.morph);
            for (k, &(_, count, _)) in jobs.iter().enumerate() {
                pass.set_bind_group(0, &self.morph_group, &[(k * align) as u32]);
                pass.dispatch_workgroups(count.div_ceil(64), 1, 1);
            }
        }

        // opaque and cut out first, then the blended ones far to near (a blended draw is one
        // primitive: its depth is that primitive's)
        let depth_of = |d: &Draw3| -> f32 {
            if !d.blended || d.samples.is_empty() {
                return 0.0;
            }
            let mut sum = Vec3::ZERO;
            for (p, j, wt) in &d.samples {
                let mut m = Mat4::ZERO;
                for c in 0..4 {
                    if wt[c] > 0.0
                        && let Some(jm) = palette.get(j[c] as usize)
                    {
                        m += *jm * wt[c];
                    }
                }
                sum += m.transform_point3(*p);
            }
            (sum / d.samples.len() as f32).distance(cam.eye)
        };
        let depths: Vec<f32> = self.draws.iter().map(depth_of).collect();
        let mut order: Vec<usize> = (0..self.draws.len()).collect();
        order.sort_by(|&x, &y| {
            let (dx, dy) = (&self.draws[x], &self.draws[y]);
            dx.blended
                .cmp(&dy.blended)
                .then(dx.queue_offset.cmp(&dy.queue_offset))
                .then(depths[y].total_cmp(&depths[x]))
        });

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("lightair-avatar-vrm"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: out,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: depth,
                depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Discard }),
                stencil_ops: None,
            }),
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_bind_group(0, &self.frame_group, &[]);
        pass.set_vertex_buffer(0, self.statics.slice(..));
        pass.set_index_buffer(self.indices.slice(..), wgpu::IndexFormat::Uint32);
        for outline in [false, true] {
            for &i in &order {
                let d = &self.draws[i];
                if d.count == 0 || (outline && !d.outline) {
                    continue;
                }
                let Some(material) = self.materials.get(d.material) else { continue };
                // back faces culled; an outline is the inflated shell's back faces only
                let pipeline = if outline {
                    &pipes.vrm[2]
                } else if d.double_sided {
                    &pipes.vrm[1]
                } else {
                    &pipes.vrm[0]
                };
                pass.set_pipeline(pipeline);
                pass.set_bind_group(1, material, &[]);
                pass.draw_indexed(d.first..d.first + d.count, 0, 0..1);
            }
        }
    }
}

/// The frame's uniforms: view and projection, view, the light's direction, the eye.
const FRAME_BYTES: u64 = 160;

/// A material's uniforms, as the libobs source sets its shader's (eight `vec4`s).
fn material_uniform(mat: &Material) -> [f32; 32] {
    let opaque = mat.alpha == AlphaMode::Opaque;
    let cutoff = match mat.alpha {
        AlphaMode::Mask(c) => c,
        _ => -1.0,
    };
    let solid = cutoff >= 0.0 || opaque;
    let (bc, sc, em, rc, oc) = (mat.base_color, mat.shade_color, mat.emissive, mat.rim_color, mat.outline_color);
    let mc = if mat.matcap_texture.is_some() { mat.matcap_factor } else { [0.0; 3] };
    [
        bc[0],
        bc[1],
        bc[2],
        if opaque { 1.0 } else { bc[3] },
        sc[0],
        sc[1],
        sc[2],
        1.0,
        em[0],
        em[1],
        em[2],
        0.0,
        rc[0],
        rc[1],
        rc[2],
        0.0,
        mc[0],
        mc[1],
        mc[2],
        0.0,
        // toony, shift, rim lift, rim fresnel power
        mat.shading_toony,
        mat.shading_shift,
        mat.rim_lift,
        mat.rim_fresnel_power,
        // cutoff (or -1), unlit, solid, outline lighting mix
        cutoff,
        if mat.unlit { 1.0 } else { 0.0 },
        if solid { 1.0 } else { 0.0 },
        mat.outline_lighting_mix,
        oc[0],
        oc[1],
        oc[2],
        mat.outline_width,
    ]
}

const VRM_SHADER: &str = r#"
struct Frame {
    view_proj: mat4x4<f32>,
    view: mat4x4<f32>,
    light_dir: vec4<f32>,
    eye_pos: vec4<f32>,
}

struct Material {
    base_color: vec4<f32>,
    shade_color: vec4<f32>,
    emissive: vec4<f32>,
    rim: vec4<f32>,
    matcap: vec4<f32>,
    // toony, shift, rim lift, rim fresnel power
    shading: vec4<f32>,
    // cutoff (or -1), unlit, solid, outline lighting mix
    extra: vec4<f32>,
    // colour, width
    outline: vec4<f32>,
}

struct Vertex {
    pos: vec4<f32>,
    nrm: vec4<f32>,
}

@group(0) @binding(0) var<uniform> frame: Frame;
@group(0) @binding(1) var<storage, read> palette: array<mat4x4<f32>>;
@group(0) @binding(2) var<storage, read> verts: array<Vertex>;
@group(1) @binding(0) var<uniform> mat: Material;
@group(1) @binding(1) var base_tex: texture_2d<f32>;
@group(1) @binding(2) var shade_tex: texture_2d<f32>;
@group(1) @binding(3) var emissive_tex: texture_2d<f32>;
@group(1) @binding(4) var matcap_tex: texture_2d<f32>;
@group(1) @binding(5) var samp: sampler;

struct In {
    @builtin(vertex_index) index: u32,
    @location(0) uv: vec2<f32>,
    @location(1) joints: vec4<u32>,
    @location(2) weights: vec4<f32>,
}

struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) norm: vec3<f32>,
    @location(2) wpos: vec3<f32>,
}

struct Posed {
    p: vec3<f32>,
    n: vec3<f32>,
}

// the vertex (morphed) skinned by its joints this frame
fn posed(v: In) -> Posed {
    var m = mat4x4<f32>(vec4<f32>(0.0), vec4<f32>(0.0), vec4<f32>(0.0), vec4<f32>(0.0));
    for (var k = 0u; k < 4u; k++) {
        let w = v.weights[k];
        if (w > 0.0) {
            m = m + palette[v.joints[k]] * w;
        }
    }
    let r = verts[v.index];
    let p = (m * vec4<f32>(r.pos.xyz, 1.0)).xyz;
    let n = (m * vec4<f32>(r.nrm.xyz, 0.0)).xyz;
    let l = length(n);
    return Posed(p, select(vec3<f32>(0.0, 0.0, 1.0), n / l, l > 1e-12));
}

@vertex
fn vs_main(v: In) -> Out {
    let q = posed(v);
    var o: Out;
    o.pos = frame.view_proj * vec4<f32>(q.p, 1.0);
    o.uv = v.uv;
    o.norm = q.n;
    o.wpos = q.p;
    return o;
}

@vertex
fn vs_outline(v: In) -> Out {
    let q = posed(v);
    let p = q.p + q.n * mat.outline.w;
    var o: Out;
    o.pos = frame.view_proj * vec4<f32>(p, 1.0);
    o.uv = v.uv;
    o.norm = q.n;
    o.wpos = p;
    return o;
}

@fragment
fn fs_main(v: Out) -> @location(0) vec4<f32> {
    // (every texture sampled first, outside any branch)
    var b = textureSample(base_tex, samp, v.uv) * mat.base_color;
    let shade_t = textureSample(shade_tex, samp, v.uv).rgb;
    let emissive_t = textureSample(emissive_tex, samp, v.uv).rgb;
    let vdir = normalize(frame.eye_pos.xyz - v.wpos);
    var n = normalize(v.norm);
    // (back faces of double-sided parts face the camera too)
    n = select(n, -n, dot(n, vdir) < 0.0);
    let vn = (frame.view * vec4<f32>(n, 0.0)).xyz;
    let muv = vec2<f32>(vn.x * 0.5 + 0.5, 0.5 - vn.y * 0.5);
    let matcap_t = textureSample(matcap_tex, samp, muv).rgb;
    if (b.a < mat.extra.x) {
        discard;
    }
    if (mat.extra.z > 0.5) {
        b.a = 1.0;
    }
    if (mat.extra.y > 0.5) {
        return b;
    }
    var s = dot(n, normalize(frame.light_dir.xyz)) + mat.shading.y;
    let lo = -1.0 + mat.shading.x;
    let hi = 1.0 - mat.shading.x;
    s = saturate((s - lo) / max(hi - lo, 0.0001));
    let shade = shade_t * mat.shade_color.rgb;
    var c = mix(shade, b.rgb, s);
    // a little ambient light into the shade
    c = mix(c, b.rgb, 0.12);
    let r = pow(saturate(1.0 - dot(n, vdir) + mat.shading.z), max(mat.shading.w, 0.001));
    c += r * mat.rim.rgb;
    c += matcap_t * mat.matcap.rgb;
    c += emissive_t * mat.emissive.rgb;
    return vec4<f32>(saturate(c), b.a);
}

@fragment
fn fs_outline(v: Out) -> @location(0) vec4<f32> {
    var b = textureSample(base_tex, samp, v.uv) * mat.base_color;
    if (b.a < mat.extra.x) {
        discard;
    }
    if (mat.extra.z > 0.5) {
        b.a = 1.0;
    }
    let c = mat.outline.rgb * mix(vec3<f32>(1.0), b.rgb, mat.extra.w);
    return vec4<f32>(c, b.a);
}
"#;

const MORPH_SHADER: &str = r#"
// a morph target's delta for one vertex: position (and the vertex's index), normal
struct Delta {
    p: vec3<f32>,
    index: u32,
    n: vec3<f32>,
    pad: u32,
}

struct Vertex {
    pos: vec4<f32>,
    nrm: vec4<f32>,
}

struct Job {
    start: u32,
    count: u32,
    weight: f32,
    pad: u32,
}

@group(0) @binding(0) var<storage, read> deltas: array<Delta>;
@group(0) @binding(1) var<storage, read_write> verts: array<Vertex>;
@group(0) @binding(2) var<uniform> job: Job;

// one target at a time (a vertex appears once in a target): its deltas, weighted, added
@compute @workgroup_size(64)
fn morph(@builtin(global_invocation_id) id: vec3<u32>) {
    if (id.x >= job.count) {
        return;
    }
    let d = deltas[job.start + id.x];
    let v = verts[d.index];
    verts[d.index] = Vertex(vec4<f32>(v.pos.xyz + d.p * job.weight, 1.0), vec4<f32>(v.nrm.xyz + d.n * job.weight, 0.0));
}
"#;

// ---------------------------------------------------------------- 2D

/// A mesh in a flat model's shared buffers.
#[derive(Clone, Copy, Default)]
struct Mesh2 {
    base: u32,
    vertices: u32,
    first: u32,
    count: u32,
}

/// A flat model (Live2D, Inochi2D, Umamo, PNGtuber) on the GPU: its meshes' texture coordinates
/// and triangles, their posed vertices (written each frame), its textures.
struct FlatGpu {
    meshes: Vec<Mesh2>,
    pos: wgpu::Buffer,
    uv: wgpu::Buffer,
    index: wgpu::Buffer,
    textures: Vec<Option<wgpu::BindGroup>>,
    /// The posed vertices, all meshes one after another (kept: a mesh not posed this frame keeps
    /// its last).
    scratch: Vec<[f32; 2]>,
}

impl FlatGpu {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue, pipes: &Pipes, f: &mut Flat) -> FlatGpu {
        let mut meshes = Vec::new();
        let mut uvs: Vec<[f32; 2]> = Vec::new();
        let mut indices: Vec<u32> = Vec::new();
        let mut add = |mesh_uvs: &mut dyn Iterator<Item = [f32; 2]>, mesh_indices: &mut dyn Iterator<Item = u32>| {
            let base = uvs.len() as u32;
            uvs.extend(mesh_uvs);
            let first = indices.len() as u32;
            indices.extend(mesh_indices);
            meshes.push(Mesh2 { base, vertices: uvs.len() as u32 - base, first, count: indices.len() as u32 - first });
        };
        // each texture with its smaller copies (made here, on the loading thread)
        let upload = |w: u32, h: u32, rgba: &[u8]| {
            let mips = mip_chain(w, h, rgba);
            upload_texture(device, queue, w, h, rgba, &mips)
                .map(|t| texture_group(device, &pipes.texture, &t.create_view(&Default::default())))
        };
        let textures: Vec<Option<wgpu::BindGroup>> = match f {
            Flat::Live2d(p) => {
                for m in p.meshes() {
                    add(&mut m.vertices().iter().map(|v| v.uv()), &mut m.indices().iter().map(|&i| u32::from(i)));
                }
                p.textures().iter().map(|t| upload(t.width(), t.height(), t.rgba())).collect()
            }
            Flat::Inochi2d(p) => {
                for part in p.parts() {
                    add(&mut part.uvs.iter().copied(), &mut part.indices.iter().copied());
                }
                p.textures().iter().map(|t| upload(t.width, t.height, &t.rgba)).collect()
            }
            Flat::Uma(p) => {
                for d in p.drawables() {
                    add(&mut d.uvs.iter().copied(), &mut d.indices.iter().copied());
                }
                p.textures().iter().map(|t| upload(t.width, t.height, &t.rgba)).collect()
            }
            // (the pictures are only needed here: the runtime keeps going without them)
            Flat::Png(p) => {
                std::mem::take(&mut p.pictures).iter().map(|pic| upload(pic.width, pic.height, &pic.rgba)).collect()
            }
        };
        let scratch = vec![[0.0; 2]; uvs.len()];
        let pos = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lightair-avatar-2d-positions"),
            size: (uvs.len().max(1) * 8) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let uv = buffer(device, "lightair-avatar-2d-uvs", bytemuck::cast_slice(&uvs), 8, wgpu::BufferUsages::VERTEX);
        let index =
            buffer(device, "lightair-avatar-2d-indices", bytemuck::cast_slice(&indices), 16, wgpu::BufferUsages::INDEX);
        FlatGpu { meshes, pos, uv, index, textures, scratch }
    }

    /// Makes the pipelines this kind of model draws with now (on the loading thread), so its
    /// first frames don't wait for them.
    fn prepare_pipelines(&self, device: &wgpu::Device, pipes: &Pipes, f: &Flat) {
        let keys: Vec<PipeKey> = match f {
            Flat::Live2d(_) => [Blend2::Normal, Blend2::Additive, Blend2::Multiply]
                .into_iter()
                .map(|b| PipeKey::new(Vs::Main, Fs::Live2d, b))
                .chain([PipeKey::new(Vs::Main, Fs::AlphaMask, Blend2::Normal)])
                .collect(),
            Flat::Uma(_) => [Blend2::Normal, Blend2::Additive, Blend2::Multiply]
                .into_iter()
                .map(|b| PipeKey::new(Vs::Main, Fs::Uma, b))
                .chain([PipeKey::new(Vs::Main, Fs::AlphaMask, Blend2::Normal)])
                .collect(),
            Flat::Inochi2d(_) => [PipeKey::new(Vs::Main, Fs::Inochi, Blend2::Normal)]
                .into_iter()
                .chain([PipeKey::new(Vs::Main, Fs::InochiMask, Blend2::Normal)])
                .chain([PipeKey::new(Vs::Main, Fs::InochiMask, Blend2::MaskDodge)])
                .collect(),
            Flat::Png(_) => vec![PipeKey::new(Vs::Main, Fs::Straight, Blend2::Normal)],
        };
        for k in keys {
            pipes.flat(device, k);
        }
    }

    /// Writes this frame's posed vertices.
    fn upload(&mut self, queue: &wgpu::Queue, f: &Flat) {
        let mut put = |i: usize, points: &mut dyn ExactSizeIterator<Item = [f32; 2]>| {
            let Some(m) = self.meshes.get(i) else { return };
            if points.len() != m.vertices as usize {
                return;
            }
            let at = m.base as usize;
            for (k, q) in points.enumerate() {
                self.scratch[at + k] = q;
            }
        };
        match f {
            Flat::Live2d(p) => {
                for (i, m) in p.meshes().iter().enumerate() {
                    put(i, &mut m.vertices().iter().map(|v| v.position()));
                }
            }
            Flat::Inochi2d(p) => {
                for (i, pos) in p.positions().iter().enumerate() {
                    put(i, &mut pos.iter().copied());
                }
            }
            Flat::Uma(p) => {
                for (i, d) in p.drawables().iter().enumerate() {
                    if d.posed {
                        put(i, &mut d.positions.iter().copied());
                    }
                }
            }
            Flat::Png(_) => return,
        }
        if !self.scratch.is_empty() {
            queue.write_buffer(&self.pos, 0, bytemuck::cast_slice(&self.scratch));
        }
    }

    /// Whether mesh `i` has triangles to draw.
    fn drawable(&self, i: usize) -> bool {
        self.meshes.get(i).is_some_and(|m| m.count > 0)
    }

    /// Whether texture `t` is on the GPU.
    fn has_texture(&self, t: usize) -> bool {
        self.textures.get(t).is_some_and(Option::is_some)
    }
}

/// A 2D draw's vertex shader.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Vs {
    Main,
    /// A DynamicComposite's mesh: its texture coordinates map onto its layer.
    Dynamic,
}

impl Vs {
    fn entry(self) -> &'static str {
        match self {
            Vs::Main => "vs_main",
            Vs::Dynamic => "vs_dynamic",
        }
    }
}

/// A 2D draw's fragment shader.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Fs {
    Live2d,
    /// A mask: the texture's alpha (Live2D, Umamo).
    AlphaMask,
    Uma,
    Inochi,
    /// A composite's layer, sampled where it lies on the frame.
    InochiLayer,
    InochiMask,
    /// A straight-alpha picture at an opacity (PNGtuber layers, the tracking view).
    Straight,
}

impl Fs {
    fn entry(self) -> &'static str {
        match self {
            Fs::Live2d => "fs_live2d",
            Fs::AlphaMask => "fs_alpha_mask",
            Fs::Uma => "fs_uma",
            Fs::Inochi => "fs_inochi",
            Fs::InochiLayer => "fs_inochi_layer",
            Fs::InochiMask => "fs_inochi_mask",
            Fs::Straight => "fs_straight",
        }
    }
}

/// How a 2D draw is laid over what's under it (premultiplied colour), as the libobs drawings
/// set libobs's blending.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Blend2 {
    /// `src + dst·(1 − src.a)`.
    Normal,
    /// Live2D and Umamo: colour added, alpha kept.
    Additive,
    /// Live2D and Umamo: colour multiplied, alpha kept.
    Multiply,
    /// Inochi2D's modes (colour and alpha alike).
    InochiMultiply,
    ColorDodge,
    LinearDodge,
    Screen,
    ClipToLower,
    SliceFromLower,
    /// Takes a mask away (Inochi2D's dodging masks).
    MaskDodge,
}

impl Blend2 {
    fn state(self) -> wgpu::BlendState {
        use wgpu::BlendFactor as F;
        use wgpu::BlendOperation as O;
        let c = |src, dst, operation| wgpu::BlendComponent { src_factor: src, dst_factor: dst, operation };
        let same = |src, dst, op| wgpu::BlendState { color: c(src, dst, op), alpha: c(src, dst, op) };
        match self {
            Blend2::Normal => same(F::One, F::OneMinusSrcAlpha, O::Add),
            Blend2::Additive => {
                wgpu::BlendState { color: c(F::One, F::One, O::Add), alpha: c(F::Zero, F::One, O::Add) }
            }
            Blend2::Multiply => {
                wgpu::BlendState { color: c(F::Dst, F::OneMinusSrcAlpha, O::Add), alpha: c(F::Zero, F::One, O::Add) }
            }
            Blend2::InochiMultiply => same(F::Dst, F::OneMinusSrcAlpha, O::Add),
            Blend2::ColorDodge => same(F::Dst, F::One, O::Add),
            Blend2::LinearDodge => same(F::One, F::One, O::Add),
            Blend2::Screen => same(F::One, F::OneMinusSrc, O::Add),
            Blend2::ClipToLower => same(F::DstAlpha, F::OneMinusSrcAlpha, O::Add),
            Blend2::SliceFromLower => same(F::OneMinusDstAlpha, F::OneMinusSrcAlpha, O::Subtract),
            Blend2::MaskDodge => same(F::Zero, F::OneMinusSrcAlpha, O::Add),
        }
    }

    fn inochi(b: inochi2d::Blend) -> Blend2 {
        match b {
            inochi2d::Blend::Normal => Blend2::Normal,
            inochi2d::Blend::Multiply => Blend2::InochiMultiply,
            inochi2d::Blend::ColorDodge => Blend2::ColorDodge,
            inochi2d::Blend::LinearDodge => Blend2::LinearDodge,
            inochi2d::Blend::Screen => Blend2::Screen,
            inochi2d::Blend::ClipToLower => Blend2::ClipToLower,
            inochi2d::Blend::SliceFromLower => Blend2::SliceFromLower,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct PipeKey {
    vs: Vs,
    fs: Fs,
    blend: Blend2,
}

impl PipeKey {
    fn new(vs: Vs, fs: Fs, blend: Blend2) -> PipeKey {
        PipeKey { vs, fs, blend }
    }
}

/// A 2D draw's uniforms (seven `vec4`s).
#[derive(Clone, Copy, Default)]
struct U2 {
    /// Model to clip space: x scale, x offset, y scale, y offset.
    proj: [f32; 4],
    /// The frame: left, top, 1 / width, 1 / height (for the mask's and layers' coordinates).
    frame: [f32; 4],
    /// Multiply colour or tint, and opacity.
    a: [f32; 4],
    /// Screen colour, and the mask's use (0 none, 1 clipped, 2 inverted).
    b: [f32; 4],
    /// Inochi2D's mask: masked, threshold, dodging.
    c: [f32; 4],
    /// A DynamicComposite's texture frame on its layer: origin, x axis; y axis.
    dyn_a: [f32; 4],
    dyn_b: [f32; 4],
}

const U2_BYTES: u64 = 112;

impl U2 {
    fn floats(&self) -> [f32; 28] {
        let mut out = [0.0; 28];
        for (k, v) in [self.proj, self.frame, self.a, self.b, self.c, self.dyn_a, self.dyn_b].iter().enumerate() {
            out[k * 4..k * 4 + 4].copy_from_slice(v);
        }
        out
    }
}

/// An orthographic projection of `left..right` across and `top..bottom` down (as libobs's
/// `gs_ortho`: `top` at the picture's top), as [`U2::proj`].
fn ortho(left: f32, right: f32, top: f32, bottom: f32) -> [f32; 4] {
    [2.0 / (right - left), -(right + left) / (right - left), 2.0 / (top - bottom), -(top + bottom) / (top - bottom)]
}

/// What a 2D draw goes into: the picture, the mask, a composite's layer, a DynamicComposite's.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Tgt {
    Out,
    Mask,
    Layer,
    Dynamic(usize),
}

/// What a 2D draw samples.
#[derive(Clone, Copy, Debug)]
enum Tex {
    Model(usize),
    Target(Tgt),
    View,
}

/// A 2D draw's triangles: a model's mesh, or a quad (six vertices) made this frame.
#[derive(Clone, Copy, Debug)]
enum Geo {
    Mesh(usize),
    Quad(u32),
}

enum Step {
    /// Drawing goes into this target from here, cleared to this value first if given.
    Target(Tgt, Option<f32>),
    Draw {
        key: PipeKey,
        geo: Geo,
        tex: Tex,
        mask: Option<Tgt>,
        uniform: u32,
    },
}

/// A frame's 2D drawing, planned before it's recorded (so its uniforms and quads are written
/// once).
#[derive(Default)]
struct Plan {
    steps: Vec<Step>,
    uniforms: Vec<U2>,
    quad_pos: Vec<[f32; 2]>,
    quad_uv: Vec<[f32; 2]>,
}

impl Plan {
    fn target(&mut self, t: Tgt, clear: Option<f32>) {
        self.steps.push(Step::Target(t, clear));
    }

    fn draw(&mut self, key: PipeKey, geo: Geo, tex: Tex, mask: Option<Tgt>, u: U2) {
        let uniform = self.uniforms.len() as u32;
        self.uniforms.push(u);
        self.steps.push(Step::Draw { key, geo, tex, mask, uniform });
    }

    /// A quad with these corners (top left, top right, bottom left, bottom right), its texture
    /// across it.
    fn quad(&mut self, corners: [[f32; 2]; 4]) -> Geo {
        let first = self.quad_pos.len() as u32;
        for k in [0, 1, 2, 2, 1, 3] {
            self.quad_pos.push(corners[k]);
            self.quad_uv.push([[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]][k]);
        }
        Geo::Quad(first)
    }

    /// The targets the plan draws into or samples.
    fn targets(&self) -> Vec<Tgt> {
        let mut out: Vec<Tgt> = Vec::new();
        let mut add = |t: Tgt| {
            if !out.contains(&t) {
                out.push(t);
            }
        };
        for s in &self.steps {
            match s {
                Step::Target(t, _) => add(*t),
                Step::Draw { tex, mask, .. } => {
                    if let Tex::Target(t) = tex {
                        add(*t);
                    }
                    if let Some(m) = mask {
                        add(*m);
                    }
                }
            }
        }
        out
    }
}

/// Where drawing goes, as libobs's texture renders keep it: each target, when begun, remembers
/// the one that was current and gives it back when ended.
struct Targeting {
    current: Tgt,
    prev: HashMap<Tgt, Tgt>,
}

impl Targeting {
    fn new() -> Targeting {
        Targeting { current: Tgt::Out, prev: HashMap::new() }
    }

    fn begin(&mut self, plan: &mut Plan, t: Tgt, clear: f32) {
        if self.current != t {
            self.prev.insert(t, self.current);
            self.current = t;
        }
        plan.target(t, Some(clear));
    }

    fn end(&mut self, plan: &mut Plan, t: Tgt) {
        self.current = self.prev.remove(&t).unwrap_or(Tgt::Out);
        plan.target(self.current, None);
    }
}

/// Plans a flat model's frame (the picture already cleared).
fn plan_flat(plan: &mut Plan, f: &Flat, g: &FlatGpu, settings: &Settings) {
    let aspect = settings.width as f32 / settings.height as f32;
    let (left, right, top, bottom) = f.frame(settings.framing, aspect, settings.zoom);
    let base = U2 {
        proj: ortho(left, right, top, bottom),
        frame: [left, top, 1.0 / (right - left), 1.0 / (top - bottom)],
        a: [1.0; 4],
        ..U2::default()
    };
    match f {
        Flat::Live2d(p) => plan_live2d(plan, p, g, base),
        Flat::Uma(p) => plan_uma(plan, p, g, base),
        Flat::Inochi2d(p) => plan_inochi(plan, p, g, base, (left, right, top, bottom)),
        Flat::Png(p) => plan_png(plan, p, g, settings),
    }
}

fn plan_live2d(plan: &mut Plan, p: &live2d::Puppet, g: &FlatGpu, base: U2) {
    let meshes = p.meshes();
    let texture = |i: usize| -> Option<usize> {
        let t = meshes.get(i)?.texture_index().max(0) as usize;
        (g.drawable(i) && g.has_texture(t)).then_some(t)
    };
    let mut masked: Option<&[i32]> = None;
    for i in p.draw_order() {
        let Some(m) = meshes.get(i) else { continue };
        if m.opacity() <= 0.001 {
            continue;
        }
        // its clip: the masks drawn into the mask (again only when they change)
        let mode = if m.masks().is_empty() {
            0.0
        } else {
            if masked != Some(m.masks()) {
                plan.target(Tgt::Mask, Some(0.0));
                for &mi in m.masks() {
                    if mi >= 0
                        && let Some(t) = texture(mi as usize)
                    {
                        let key = PipeKey::new(Vs::Main, Fs::AlphaMask, Blend2::Normal);
                        plan.draw(key, Geo::Mesh(mi as usize), Tex::Model(t), None, base);
                    }
                }
                plan.target(Tgt::Out, None);
                masked = Some(m.masks());
            }
            if m.is_inverted_mask() { 2.0 } else { 1.0 }
        };
        let Some(t) = texture(i) else { continue };
        let (mul, scr) = (m.multiply_color(), m.screen_color());
        let blend = match m.blend_mode() {
            Moc3DrawableBlendMode::Normal => Blend2::Normal,
            Moc3DrawableBlendMode::Additive => Blend2::Additive,
            Moc3DrawableBlendMode::Multiplicative => Blend2::Multiply,
        };
        let u = U2 { a: [mul[0], mul[1], mul[2], m.opacity()], b: [scr[0], scr[1], scr[2], mode], ..base };
        let mask = (mode > 0.0).then_some(Tgt::Mask);
        plan.draw(PipeKey::new(Vs::Main, Fs::Live2d, blend), Geo::Mesh(i), Tex::Model(t), mask, u);
    }
}

fn plan_uma(plan: &mut Plan, p: &uma::Puppet, g: &FlatGpu, base: U2) {
    let drawables = p.drawables();
    let texture = |i: usize| -> Option<usize> {
        let d = drawables.get(i)?;
        (d.posed && g.drawable(i) && g.has_texture(d.texture)).then_some(d.texture)
    };
    let mut masked: Option<&[usize]> = None;
    for i in p.draw_order() {
        let Some(d) = drawables.get(i) else { continue };
        if !d.posed || d.opacity <= 0.001 {
            continue;
        }
        let mode = if d.masks.is_empty() {
            0.0
        } else {
            if masked != Some(d.masks.as_slice()) {
                plan.target(Tgt::Mask, Some(0.0));
                for &mi in &d.masks {
                    if let Some(t) = texture(mi) {
                        let key = PipeKey::new(Vs::Main, Fs::AlphaMask, Blend2::Normal);
                        plan.draw(key, Geo::Mesh(mi), Tex::Model(t), None, base);
                    }
                }
                plan.target(Tgt::Out, None);
                masked = Some(d.masks.as_slice());
            }
            if d.inverted_mask { 2.0 } else { 1.0 }
        };
        let Some(t) = texture(i) else { continue };
        let blend = match d.blend {
            uma::Blend::Normal => Blend2::Normal,
            uma::Blend::Additive => Blend2::Additive,
            uma::Blend::Multiply => Blend2::Multiply,
        };
        let (mul, scr) = (d.multiply, d.screen);
        let u = U2 { a: [mul[0], mul[1], mul[2], d.opacity], b: [scr[0], scr[1], scr[2], mode], ..base };
        let mask = (mode > 0.0).then_some(Tgt::Mask);
        plan.draw(PipeKey::new(Vs::Main, Fs::Uma, blend), Geo::Mesh(i), Tex::Model(t), mask, u);
    }
}

fn plan_inochi(plan: &mut Plan, p: &inochi2d::Puppet, g: &FlatGpu, base: U2, frame: (f32, f32, f32, f32)) {
    let (left, right, top, bottom) = frame;
    let with_look = |look: &Look, masked: bool, threshold: f32| U2 {
        a: [look.tint[0], look.tint[1], look.tint[2], look.opacity],
        b: [look.screen[0], look.screen[1], look.screen[2], 0.0],
        c: [if masked { 1.0 } else { 0.0 }, threshold, 0.0, 0.0],
        ..base
    };
    let mut layer_quad: Option<Geo> = None;
    let mut targets = Targeting::new();
    // drawing into the mask (between BeginMasks and MaskedContent), drawing with it, drawing into
    // a composite's layer
    let (mut masking, mut masked, mut dodge, mut threshold, mut composite) = (false, false, false, 0.0f32, false);
    // DynamicComposites open: each with whether it got a layer
    let mut dynamics: Vec<bool> = Vec::new();
    for op in p.ops() {
        match op {
            Op::BeginMasks { threshold: th, clear } => {
                threshold = *th;
                masked = false;
                targets.begin(plan, Tgt::Mask, *clear);
                masking = true;
            }
            Op::Mask { dodge: d } => dodge = *d,
            Op::MaskedContent => {
                if masking {
                    targets.end(plan, Tgt::Mask);
                    masking = false;
                    masked = true;
                }
            }
            Op::EndMasks => {
                if masking {
                    targets.end(plan, Tgt::Mask);
                    masking = false;
                }
                masked = false;
            }
            Op::BeginComposite { as_mask: false } => {
                targets.begin(plan, Tgt::Layer, 0.0);
                composite = true;
            }
            Op::EndComposite { as_mask: false, look } => {
                if composite {
                    targets.end(plan, Tgt::Layer);
                    composite = false;
                    let quad = *layer_quad
                        .get_or_insert_with(|| plan.quad([[left, top], [right, top], [left, bottom], [right, bottom]]));
                    let key = PipeKey::new(Vs::Main, Fs::InochiLayer, Blend2::inochi(look.blend));
                    let mask = masked.then_some(Tgt::Mask);
                    plan.draw(key, quad, Tex::Target(Tgt::Layer), mask, with_look(look, masked, threshold));
                }
            }
            // a composite as a mask: its children go straight into the mask
            Op::BeginComposite { as_mask: true } | Op::EndComposite { as_mask: true, .. } => {}
            Op::BeginDynamic { as_mask: false } => {
                let level = dynamics.len();
                let begun = level < DYNAMIC_LEVELS;
                if begun {
                    targets.begin(plan, Tgt::Dynamic(level), 0.0);
                }
                dynamics.push(begun);
            }
            Op::EndDynamic { as_mask: false, part, look, texture } => {
                let level = dynamics.len().saturating_sub(1);
                if dynamics.pop() == Some(true) {
                    targets.end(plan, Tgt::Dynamic(level));
                    // the texture frame on the layer (u right, v down, 0..1 across the frame)
                    let TextureFrame { origin, x_axis, y_axis } = *texture;
                    let (sx, sy) = (1.0 / (right - left), 1.0 / (top - bottom));
                    let o = [(origin[0] - left) * sx, (top - origin[1]) * sy];
                    let (xa, ya) = ([x_axis[0] * sx, -x_axis[1] * sy], [y_axis[0] * sx, -y_axis[1] * sy]);
                    if g.drawable(*part) {
                        let m = masked && !composite;
                        let u = U2 {
                            dyn_a: [o[0], o[1], xa[0], xa[1]],
                            dyn_b: [ya[0], ya[1], 0.0, 0.0],
                            ..with_look(look, m, threshold)
                        };
                        let key = PipeKey::new(Vs::Dynamic, Fs::Inochi, Blend2::inochi(look.blend));
                        plan.draw(key, Geo::Mesh(*part), Tex::Target(Tgt::Dynamic(level)), m.then_some(Tgt::Mask), u);
                    }
                }
            }
            Op::BeginDynamic { as_mask: true } | Op::EndDynamic { as_mask: true, .. } => {}
            Op::Part { part, as_mask, look } => {
                let Some(t) = p.parts().get(*part).map(|pt| pt.texture) else { continue };
                if !g.drawable(*part) || !g.has_texture(t) {
                    continue;
                }
                if *as_mask || masking {
                    if !masking {
                        continue;
                    }
                    let u = U2 { c: [0.0, threshold, if dodge { 1.0 } else { 0.0 }, 0.0], ..base };
                    let blend = if dodge { Blend2::MaskDodge } else { Blend2::Normal };
                    plan.draw(PipeKey::new(Vs::Main, Fs::InochiMask, blend), Geo::Mesh(*part), Tex::Model(t), None, u);
                } else {
                    // (a composite's children aren't masked; the layer is)
                    let m = masked && !composite;
                    let key = PipeKey::new(Vs::Main, Fs::Inochi, Blend2::inochi(look.blend));
                    plan.draw(
                        key,
                        Geo::Mesh(*part),
                        Tex::Model(t),
                        m.then_some(Tgt::Mask),
                        with_look(look, m, threshold),
                    );
                }
            }
        }
    }
}

fn plan_png(plan: &mut Plan, p: &Pngtuber, g: &FlatGpu, settings: &Settings) {
    let a = p.puppet.avatar();
    let ch = a.height as f32;
    let aspect = settings.width as f32 / settings.height as f32;
    let (left, right, top, bottom) = p.frame(settings.framing, aspect, settings.zoom);
    // the canvas y down: the frame's top edge at the picture's top
    let base = U2 { proj: ortho(left, right, ch - top, ch - bottom), a: [1.0; 4], ..U2::default() };
    let pose = p.puppet.pose();
    for (i, layer) in a.layers.iter().enumerate() {
        let Some(lp) = pose.layers.get(i) else { continue };
        if !lp.visible || lp.opacity <= 0.001 || !g.has_texture(i) {
            continue;
        }
        let [x, y] = lp.place([layer.x, layer.y]);
        let (lw, lh) = (layer.width * lp.sx, layer.height * lp.sy);
        let quad = plan.quad([[x, y], [x + lw, y], [x, y + lh], [x + lw, y + lh]]);
        let u = U2 { a: [1.0, 1.0, 1.0, lp.opacity], ..base };
        plan.draw(PipeKey::new(Vs::Main, Fs::Straight, Blend2::Normal), quad, Tex::Model(i), None, u);
    }
}

/// Plans the tracking view (a `width`×`height` picture) in the lower left corner, over the
/// avatar.
fn plan_view(plan: &mut Plan, settings: &Settings, (width, height): (u32, u32)) {
    if width == 0 || height == 0 {
        return;
    }
    let (w, h) = (settings.width as f32, settings.height as f32);
    let vw = (w * 0.38).round();
    let vh = (vw * height as f32 / width as f32).round();
    let (x, y) = (12.0, h - vh - 12.0);
    plan.target(Tgt::Out, None);
    let quad = plan.quad([[x, y], [x + vw, y], [x, y + vh], [x + vw, y + vh]]);
    let u = U2 { proj: ortho(0.0, w, 0.0, h), a: [1.0; 4], ..U2::default() };
    plan.draw(PipeKey::new(Vs::Main, Fs::Straight, Blend2::Normal), quad, Tex::View, None, u);
}

const FLAT_SHADER: &str = r#"
struct Draw {
    // model to clip space: x scale, x offset, y scale, y offset
    proj: vec4<f32>,
    // left, top, 1 / width, 1 / height
    frame: vec4<f32>,
    // multiply colour or tint, opacity
    a: vec4<f32>,
    // screen colour, the mask's use (0 none, 1 clipped, 2 inverted)
    b: vec4<f32>,
    // Inochi2D's mask: masked, threshold, dodging
    c: vec4<f32>,
    dyn_a: vec4<f32>,
    dyn_b: vec4<f32>,
}

@group(0) @binding(0) var<uniform> d: Draw;
@group(0) @binding(1) var samp: sampler;
@group(1) @binding(0) var image: texture_2d<f32>;
@group(2) @binding(0) var mask: texture_2d<f32>;

struct In {
    @location(0) pos: vec2<f32>,
    @location(1) uv: vec2<f32>,
}

struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    // where the point is on the frame (the mask's and layers' coordinates)
    @location(1) muv: vec2<f32>,
}

fn place(p: vec2<f32>) -> vec4<f32> {
    return vec4<f32>(p.x * d.proj.x + d.proj.y, p.y * d.proj.z + d.proj.w, 0.0, 1.0);
}

fn on_frame(p: vec2<f32>) -> vec2<f32> {
    return vec2<f32>((p.x - d.frame.x) * d.frame.z, (d.frame.y - p.y) * d.frame.w);
}

@vertex
fn vs_main(v: In) -> Out {
    var o: Out;
    o.pos = place(v.pos);
    o.uv = v.uv;
    o.muv = on_frame(v.pos);
    return o;
}

// a DynamicComposite's mesh: its UVs say where on the frame (so on its layer) each point shows
@vertex
fn vs_dynamic(v: In) -> Out {
    var o: Out;
    o.pos = place(v.pos);
    o.uv = d.dyn_a.xy + v.uv.x * d.dyn_a.zw + v.uv.y * d.dyn_b.xy;
    o.muv = on_frame(v.pos);
    return o;
}

// how much of a clipped mesh shows
fn clipped(m: f32) -> f32 {
    if (d.b.a > 1.5) {
        return 1.0 - m;
    }
    if (d.b.a > 0.5) {
        return m;
    }
    return 1.0;
}

// Live2D: straight texels, multiplied then screened, faded and clipped; premultiplied out
@fragment
fn fs_live2d(v: Out) -> @location(0) vec4<f32> {
    let t = textureSample(image, samp, v.uv);
    let m = textureSample(mask, samp, v.muv).a;
    let a = t.a * d.a.a * clipped(m);
    var c = t.rgb * d.a.rgb;
    c = c + d.b.rgb - c * d.b.rgb;
    return vec4<f32>(c * a, a);
}

// a mask: the texture's alpha
@fragment
fn fs_alpha_mask(v: Out) -> @location(0) vec4<f32> {
    let a = textureSample(image, samp, v.uv).a;
    return vec4<f32>(a);
}

// Umamo: premultiplied texels, their colour multiplied then screened, faded and clipped
@fragment
fn fs_uma(v: Out) -> @location(0) vec4<f32> {
    let t = textureSample(image, samp, v.uv);
    let m = textureSample(mask, samp, v.muv).a;
    let k = d.a.a * clipped(m);
    var c = t.rgb * d.a.rgb;
    c = c + d.b.rgb * t.a - c * d.b.rgb;
    return vec4<f32>(c, t.a) * k;
}

// Inochi2D: premultiplied colour, screened then tinted, faded; masked where a mask is set
fn look(t: vec4<f32>, m: f32) -> vec4<f32> {
    let screened = 1.0 - (1.0 - t.rgb) * (1.0 - d.b.rgb * t.a);
    var c = vec4<f32>(screened * d.a.rgb, t.a) * d.a.a;
    if (d.c.x > 0.5) {
        c = c * m;
    }
    return c;
}

@fragment
fn fs_inochi(v: Out) -> @location(0) vec4<f32> {
    let t = textureSample(image, samp, v.uv);
    let m = textureSample(mask, samp, v.muv).a;
    return look(t, m);
}

// a layer the frame's size, sampled where it lies
@fragment
fn fs_inochi_layer(v: Out) -> @location(0) vec4<f32> {
    let t = textureSample(image, samp, v.muv);
    let m = textureSample(mask, samp, v.muv).a;
    return look(t, m);
}

// a mask: set (or, dodging, cleared) where the texture's alpha is over the threshold
@fragment
fn fs_inochi_mask(v: Out) -> @location(0) vec4<f32> {
    let a = textureSample(image, samp, v.uv).a;
    let m = select(0.0, 1.0, a > d.c.y);
    return select(vec4<f32>(m), vec4<f32>(0.0, 0.0, 0.0, m), d.c.z > 0.5);
}

// a straight-alpha picture at an opacity; premultiplied out
@fragment
fn fs_straight(v: Out) -> @location(0) vec4<f32> {
    let c = textureSample(image, samp, v.uv);
    let a = c.a * d.a.a;
    return vec4<f32>(c.rgb * a, a);
}
"#;

// ---------------------------------------------------------------- targets and recording

/// A texture drawn into and sampled.
struct Aux {
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    group: wgpu::BindGroup,
}

impl Aux {
    fn new(device: &wgpu::Device, pipes: &Pipes, (width, height): (u32, u32), label: &str) -> Aux {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(label),
            size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&Default::default());
        let group = texture_group(device, &pipes.texture, &view);
        Aux { texture, view, group }
    }
}

/// The textures an avatar is drawn into at its size: two pictures (drawn into in turn, so the
/// one the compositor was last given isn't drawn over while it may still be in use), the depth,
/// and the mask and layers the flat models need (made when first used).
struct Targets {
    size: (u32, u32),
    out: [Aux; 2],
    /// The picture drawn into this frame.
    next: usize,
    depth: Option<wgpu::TextureView>,
    mask: Option<Aux>,
    layer: Option<Aux>,
    dynamic: [Option<Aux>; DYNAMIC_LEVELS],
}

impl Targets {
    fn new(device: &wgpu::Device, pipes: &Pipes, size: (u32, u32)) -> Targets {
        Targets {
            size,
            out: [
                Aux::new(device, pipes, size, "lightair-avatar-picture"),
                Aux::new(device, pipes, size, "lightair-avatar-picture"),
            ],
            next: 0,
            depth: None,
            mask: None,
            layer: None,
            dynamic: Default::default(),
        }
    }

    fn depth(&mut self, device: &wgpu::Device) -> wgpu::TextureView {
        let (width, height) = self.size;
        self.depth
            .get_or_insert_with(|| {
                device
                    .create_texture(&wgpu::TextureDescriptor {
                        label: Some("lightair-avatar-depth"),
                        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
                        mip_level_count: 1,
                        sample_count: 1,
                        dimension: wgpu::TextureDimension::D2,
                        format: DEPTH,
                        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                        view_formats: &[],
                    })
                    .create_view(&Default::default())
            })
            .clone()
    }

    fn slot(&mut self, t: Tgt) -> Option<&mut Option<Aux>> {
        match t {
            Tgt::Out => None,
            Tgt::Mask => Some(&mut self.mask),
            Tgt::Layer => Some(&mut self.layer),
            Tgt::Dynamic(level) => self.dynamic.get_mut(level),
        }
    }

    fn ensure(&mut self, device: &wgpu::Device, pipes: &Pipes, t: Tgt) {
        let size = self.size;
        if let Some(slot) = self.slot(t)
            && slot.is_none()
        {
            *slot = Some(Aux::new(device, pipes, size, "lightair-avatar-layer"));
        }
    }

    fn aux(&self, t: Tgt) -> Option<&Aux> {
        match t {
            Tgt::Out => Some(&self.out[self.next]),
            Tgt::Mask => self.mask.as_ref(),
            Tgt::Layer => self.layer.as_ref(),
            Tgt::Dynamic(level) => self.dynamic.get(level).and_then(Option::as_ref),
        }
    }
}

/// What a renderer reuses frame to frame for its 2D drawing: the draws' uniforms, the quads,
/// the tracking view's texture.
struct Scratch {
    stride: u64,
    uniforms: wgpu::Buffer,
    uniform_capacity: usize,
    group: wgpu::BindGroup,
    quad_pos: wgpu::Buffer,
    quad_uv: wgpu::Buffer,
    quad_capacity: usize,
    view: Option<(wgpu::Texture, wgpu::BindGroup)>,
    /// The tracking view's picture's size.
    view_size: (u32, u32),
    view_pixels: Vec<u8>,
}

impl Scratch {
    fn new(device: &wgpu::Device, pipes: &Pipes) -> Scratch {
        let stride = U2_BYTES.div_ceil(pipes.uniform_align) * pipes.uniform_align;
        let (uniforms, group) = Scratch::uniform_buffer(device, pipes, stride, 64);
        let (quad_pos, quad_uv) = Scratch::quad_buffers(device, 64);
        Scratch {
            stride,
            uniforms,
            uniform_capacity: 64,
            group,
            quad_pos,
            quad_uv,
            quad_capacity: 64,
            view: None,
            view_size: (0, 0),
            view_pixels: Vec::new(),
        }
    }

    fn uniform_buffer(
        device: &wgpu::Device,
        pipes: &Pipes,
        stride: u64,
        capacity: usize,
    ) -> (wgpu::Buffer, wgpu::BindGroup) {
        let uniforms = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("lightair-avatar-2d-uniforms"),
            size: stride * capacity as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("lightair-avatar-2d-uniforms"),
            layout: &pipes.flat_draw,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &uniforms,
                        offset: 0,
                        size: NonZeroU64::new(U2_BYTES),
                    }),
                },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&pipes.clamp) },
            ],
        });
        (uniforms, group)
    }

    fn quad_buffers(device: &wgpu::Device, capacity: usize) -> (wgpu::Buffer, wgpu::Buffer) {
        let make = |label| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (capacity * 8) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        (make("lightair-avatar-quad-positions"), make("lightair-avatar-quad-uvs"))
    }

    /// Writes the plan's uniforms and quads (the buffers grown as needed).
    fn prepare(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, pipes: &Pipes, plan: &Plan) {
        if plan.uniforms.len() > self.uniform_capacity {
            let capacity = plan.uniforms.len().next_power_of_two();
            (self.uniforms, self.group) = Scratch::uniform_buffer(device, pipes, self.stride, capacity);
            self.uniform_capacity = capacity;
        }
        if plan.quad_pos.len() > self.quad_capacity {
            let capacity = plan.quad_pos.len().next_power_of_two();
            (self.quad_pos, self.quad_uv) = Scratch::quad_buffers(device, capacity);
            self.quad_capacity = capacity;
        }
        if !plan.uniforms.is_empty() {
            let stride = self.stride as usize;
            let mut bytes = vec![0u8; plan.uniforms.len() * stride];
            for (k, u) in plan.uniforms.iter().enumerate() {
                bytes[k * stride..k * stride + U2_BYTES as usize].copy_from_slice(bytemuck::cast_slice(&u.floats()));
            }
            queue.write_buffer(&self.uniforms, 0, &bytes);
        }
        if !plan.quad_pos.is_empty() {
            queue.write_buffer(&self.quad_pos, 0, bytemuck::cast_slice(&plan.quad_pos));
            queue.write_buffer(&self.quad_uv, 0, bytemuck::cast_slice(&plan.quad_uv));
        }
    }

    /// The tracking view's texture for this frame: the trackers' picture with the face's points
    /// green, the body's joints red (found in 3D) or orange (the picture's only), low-confidence
    /// ones grey, the hands yellow, mirrored with the avatar. `None` without a picture.
    fn view(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipes: &Pipes,
        v: &TrackingView,
        mirror: bool,
    ) -> Option<wgpu::BindGroup> {
        let (w, h) = (v.width, v.height);
        let row = w as usize * 4;
        if w == 0 || h == 0 || v.bgra.len() < row * h as usize {
            return None;
        }
        let px = &mut self.view_pixels;
        px.clear();
        px.extend_from_slice(&v.bgra[..row * h as usize]);
        // (dots on the picture as it is, then both flipped together)
        let mut dot = |x: f32, y: f32, r: i32, c: [u8; 3]| {
            let (cx, cy) = ((x * w as f32) as i32, (y * h as f32) as i32);
            for dy in -r..=r {
                for dx in -r..=r {
                    let (xx, yy) = (cx + dx, cy + dy);
                    if xx >= 0 && yy >= 0 && (xx as u32) < w && (yy as u32) < h {
                        let i = (yy as usize * w as usize + xx as usize) * 4;
                        px[i..i + 4].copy_from_slice(&[c[2], c[1], c[0], 255]);
                    }
                }
            }
        };
        for p in v.face {
            dot(p[0], p[1], 0, [80, 255, 120]);
        }
        if let Some(b) = v.body {
            let colour = if b.joints3d.is_some() { [255, 60, 60] } else { [255, 160, 40] };
            for (p, c) in b.joints2d.iter().flatten() {
                if *c >= 0.3 {
                    dot(p.x, p.y, 4, colour);
                } else if *c > 0.0 {
                    dot(p.x, p.y, 2, [120, 120, 120]);
                }
            }
            for hand in b.hands.iter().flatten() {
                for (p, c) in hand {
                    if *c > 0.2 {
                        dot(p.x, p.y, 2, [255, 230, 60]);
                    }
                }
            }
        }
        if mirror {
            for line in px.chunks_exact_mut(row) {
                for x in 0..w as usize / 2 {
                    for k in 0..4 {
                        line.swap(x * 4 + k, (w as usize - 1 - x) * 4 + k);
                    }
                }
            }
        }
        if self.view_size != (w, h) || self.view.is_none() {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("lightair-avatar-tracking-view"),
                size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Bgra8Unorm,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let group = texture_group(device, &pipes.texture, &texture.create_view(&Default::default()));
            self.view = Some((texture, group));
            self.view_size = (w, h);
        }
        let (texture, group) = self.view.as_ref()?;
        queue.write_texture(
            texture.as_image_copy(),
            &self.view_pixels,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        Some(group.clone())
    }
}

/// Records a plan: a render pass each time the target changes (cleared when the plan says so),
/// then its draws.
#[allow(clippy::too_many_arguments)]
fn run(
    device: &wgpu::Device,
    encoder: &mut wgpu::CommandEncoder,
    plan: &Plan,
    pipes: &Pipes,
    scratch: &Scratch,
    targets: &Targets,
    flat: Option<&FlatGpu>,
    view: Option<&wgpu::BindGroup>,
) {
    let mut pass: Option<wgpu::RenderPass<'static>> = None;
    let mut current = Tgt::Out;
    for step in &plan.steps {
        match *step {
            Step::Target(t, clear) => {
                drop(pass.take());
                current = t;
                let Some(aux) = targets.aux(t) else { continue };
                let load = clear.map_or(wgpu::LoadOp::Load, |c| {
                    let c = f64::from(c);
                    wgpu::LoadOp::Clear(wgpu::Color { r: c, g: c, b: c, a: c })
                });
                let p = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("lightair-avatar-2d"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view: &aux.view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations { load, store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: None,
                    timestamp_writes: None,
                    occlusion_query_set: None,
                    multiview_mask: None,
                });
                pass = Some(p.forget_lifetime());
            }
            Step::Draw { key, geo, tex, mask, uniform } => {
                let Some(p) = pass.as_mut() else { continue };
                // (a target can't be sampled while it's drawn into)
                let image = match tex {
                    Tex::Model(i) => flat.and_then(|f| f.textures.get(i)).and_then(Option::as_ref),
                    Tex::Target(t) if t == current => None,
                    Tex::Target(t) => targets.aux(t).map(|a| &a.group),
                    Tex::View => view,
                };
                let Some(image) = image else { continue };
                let mask = match mask {
                    Some(m) if m != current => targets.aux(m).map_or(&pipes.white_group, |a| &a.group),
                    _ => &pipes.white_group,
                };
                p.set_pipeline(&pipes.flat(device, key));
                p.set_bind_group(0, &scratch.group, &[(u64::from(uniform) * scratch.stride) as u32]);
                p.set_bind_group(1, image, &[]);
                p.set_bind_group(2, mask, &[]);
                match geo {
                    Geo::Mesh(i) => {
                        let Some(f) = flat else { continue };
                        let Some(m) = f.meshes.get(i) else { continue };
                        p.set_vertex_buffer(0, f.pos.slice(..));
                        p.set_vertex_buffer(1, f.uv.slice(..));
                        p.set_index_buffer(f.index.slice(..), wgpu::IndexFormat::Uint32);
                        p.draw_indexed(m.first..m.first + m.count, m.base as i32, 0..1);
                    }
                    Geo::Quad(first) => {
                        p.set_vertex_buffer(0, scratch.quad_pos.slice(..));
                        p.set_vertex_buffer(1, scratch.quad_uv.slice(..));
                        p.draw(first..first + 6, 0..1);
                    }
                }
            }
        }
    }
}
