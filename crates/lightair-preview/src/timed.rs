//! Effects that work across frames, drawn by the compositor on a source's picture before the
//! renderer's own effects (macOS, with `LIGHTAIR_ENGINE=own`):
//!
//! - **Freeze** (`lightair_freeze`): `frozen` holds the picture from the moment it's switched on
//!   (and again whenever the source is shown again); `hold_ms` thaws it after that long (0: until
//!   switched off); `fade` lets that much of the live picture through, under the frozen one.
//! - **Render delay** (`gpu_delay`): the picture `delay_ms` ago, from a ring of copies.
//! - **Scroll** (`scroll_filter`): the picture moving `speed_x`, `speed_y` pixels a second,
//!   wrapping round with `loop` (on by default), else moving off and leaving it transparent.
//!
//! They're drawn here only while they come before every other picture effect of the source (the
//! order libobs runs a source's filters in); otherwise libobs draws the source, filters and all.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use lightair_engine::gpu::Gpu;
use lightair_model::{Effect, Id, Source};
use lightair_obs::bridge;
use lightair_render::media::Picture;
use lightair_render::scene::Shot;

pub(crate) const FREEZE: &str = "lightair_freeze";
pub(crate) const DELAY: &str = "gpu_delay";
pub(crate) const SCROLL: &str = "scroll_filter";

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// The most copies a delay keeps (half a second at 60 frames a second, and a little more).
const MOST_DELAYED: usize = 40;

const SHADER: &str = r#"
struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

struct P { offset: vec2<f32>, wrap: f32, opacity: f32 };
@group(0) @binding(0) var<uniform> p: P;
@group(0) @binding(1) var picture: texture_2d<f32>;
@group(0) @binding(2) var linear: sampler;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> Out {
    let q = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: Out;
    o.pos = vec4<f32>(q * 2.0 - 1.0, 0.0, 1.0);
    o.uv = vec2<f32>(q.x, 1.0 - q.y);
    return o;
}

@fragment
fn fs(v: Out) -> @location(0) vec4<f32> {
    var uv = v.uv + p.offset;
    if (p.wrap > 0.5) {
        uv = fract(uv);
    } else if (uv.x < 0.0 || uv.y < 0.0 || uv.x > 1.0 || uv.y > 1.0) {
        return vec4<f32>(0.0);
    }
    return textureSampleLevel(picture, linear, uv, 0.0) * p.opacity;
}
"#;

/// Whether `e` is one of the effects drawn here.
pub(crate) fn is_timed(e: &Effect) -> bool {
    matches!(e.kind.as_str(), FREEZE | DELAY | SCROLL)
}

/// The pipelines, made once: a plain copy and one blended over what's there (premultiplied).
struct Pass {
    copy: wgpu::RenderPipeline,
    over: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

impl Pass {
    fn new(device: &wgpu::Device) -> Pass {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("timed"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("timed"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("timed"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = |blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("timed"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some("fs"),
                    compilation_options: Default::default(),
                    targets: &[Some(wgpu::ColorTargetState {
                        format: FORMAT,
                        blend,
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("timed"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Pass {
            copy: pipeline(None),
            over: pipeline(Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING)),
            layout,
            sampler,
        }
    }
}

/// One draw into a target: a picture, moved by `offset` (a fraction of its size), wrapping or
/// not, at `opacity`, replacing what's there or over it.
struct Draw<'a> {
    from: &'a wgpu::Texture,
    offset: (f32, f32),
    wrap: bool,
    opacity: f32,
    over: bool,
}

/// A source's state across frames.
#[derive(Default)]
struct State {
    /// The frozen picture, and when it froze; `None` while not frozen.
    frozen: Option<(wgpu::Texture, Instant)>,
    /// Freeze is on (a change to on freezes again).
    freezing: bool,
    /// The delay's copies, oldest first, with when each was taken.
    delayed: VecDeque<(Instant, wgpu::Texture)>,
    /// Copies free to reuse.
    spare: Vec<wgpu::Texture>,
    /// The scroll's offset (pixels) and when it last moved.
    scrolled: (f32, f32),
    moved: Option<Instant>,
    /// What each effect draws into, in turn.
    outs: Vec<wgpu::Texture>,
}

/// The compositor's effects that work across frames, by source.
#[derive(Default)]
pub(crate) struct Timed {
    pass: Option<Pass>,
    states: HashMap<Id, State>,
}

impl Timed {
    /// Lets go of the state of sources not in `shown` (a frozen source shown again freezes anew).
    pub(crate) fn keep(&mut self, shown: &HashSet<Id>) {
        self.states.retain(|id, _| shown.contains(id));
    }

    /// `shot` (of `buffer`, `source`'s picture, or a texture made here) with `source`'s timed
    /// effects drawn on it, in order; `None` when it has none on, or the GPU couldn't.
    pub(crate) fn draw(
        &mut self,
        gpu: &Gpu,
        source: &Source,
        shot: &Shot,
        buffer: Option<&bridge::Buffer>,
    ) -> Option<Shot> {
        let effects: Vec<&Effect> = source.effects.iter().filter(|e| e.enabled && is_timed(e)).collect();
        if effects.is_empty() {
            self.states.remove(&source.id);
            return None;
        }
        let device = gpu.device();
        let pass = self.pass.get_or_insert_with(|| Pass::new(device));
        let mut current = match &shot.picture {
            Picture::Texture(t) => t.clone(),
            Picture::Buffer(_) => lightair_engine::gpu::apple::sampled(device, buffer?)?,
            Picture::Rgba(_) => return None,
        };
        let size = (current.width(), current.height());
        let shown_size = shot.size;
        let state = self.states.entry(source.id).or_default();
        let now = Instant::now();
        for (n, e) in effects.iter().enumerate() {
            let num = |k: &str, d: f64| e.settings.get(k).and_then(serde_json::Value::as_f64).unwrap_or(d);
            let flag = |k: &str, d: bool| e.settings.get(k).and_then(serde_json::Value::as_bool).unwrap_or(d);
            while state.outs.len() <= n {
                state.outs.push(texture(device, size));
            }
            if (state.outs[n].width(), state.outs[n].height()) != size {
                state.outs[n] = texture(device, size);
            }
            let out = state.outs[n].clone();
            match e.kind.as_str() {
                FREEZE => {
                    let on = flag("frozen", true);
                    if on && !state.freezing {
                        state.frozen = None;
                    }
                    state.freezing = on;
                    if !on {
                        state.frozen = None;
                        continue;
                    }
                    let hold = num("hold_ms", 0.0).max(0.0) as u64;
                    let fade = num("fade", 0.0).clamp(0.0, 1.0) as f32;
                    if state.frozen.as_ref().is_none_or(|(t, _)| (t.width(), t.height()) != size) {
                        let held = take(&mut state.spare, device, size);
                        run(
                            gpu,
                            pass,
                            &held,
                            &[Draw { from: &current, offset: (0.0, 0.0), wrap: false, opacity: 1.0, over: false }],
                        );
                        state.frozen = Some((held, now));
                    }
                    let (held, since) = state.frozen.as_ref()?;
                    if hold > 0 && now.duration_since(*since) >= Duration::from_millis(hold) {
                        // thawed: the live picture
                        continue;
                    }
                    let mut draws = Vec::new();
                    if fade > 0.0 {
                        draws.push(Draw { from: &current, offset: (0.0, 0.0), wrap: false, opacity: 1.0, over: false });
                    }
                    draws.push(Draw {
                        from: held,
                        offset: (0.0, 0.0),
                        wrap: false,
                        opacity: 1.0 - fade,
                        over: fade > 0.0,
                    });
                    run(gpu, pass, &out, &draws);
                }
                DELAY => {
                    let delay = Duration::from_millis(num("delay_ms", 0.0).clamp(0.0, 500.0) as u64);
                    // this frame's picture into the ring
                    let copy = take(&mut state.spare, device, size);
                    run(
                        gpu,
                        pass,
                        &copy,
                        &[Draw { from: &current, offset: (0.0, 0.0), wrap: false, opacity: 1.0, over: false }],
                    );
                    state.delayed.push_back((now, copy));
                    // the newest copy at least `delay` old; older ones go back to the spare ones
                    while state.delayed.len() > 1 && now.duration_since(state.delayed[1].0) >= delay {
                        if let Some((_, old)) = state.delayed.pop_front() {
                            state.spare.push(old);
                        }
                    }
                    while state.delayed.len() > MOST_DELAYED {
                        if let Some((_, old)) = state.delayed.pop_front() {
                            state.spare.push(old);
                        }
                    }
                    let (_, shown) = state.delayed.front()?;
                    run(
                        gpu,
                        pass,
                        &out,
                        &[Draw { from: shown, offset: (0.0, 0.0), wrap: false, opacity: 1.0, over: false }],
                    );
                }
                _ => {
                    // scroll
                    let dt = state.moved.map_or(0.0, |t| now.duration_since(t).as_secs_f32());
                    state.moved = Some(now);
                    let (sx, sy) = (num("speed_x", 50.0) as f32, num("speed_y", 0.0) as f32);
                    let wrap = flag("loop", true);
                    state.scrolled.0 += sx * dt;
                    state.scrolled.1 += sy * dt;
                    if wrap {
                        state.scrolled.0 %= size.0.max(1) as f32;
                        state.scrolled.1 %= size.1.max(1) as f32;
                    }
                    let offset = (-state.scrolled.0 / size.0.max(1) as f32, -state.scrolled.1 / size.1.max(1) as f32);
                    run(gpu, pass, &out, &[Draw { from: &current, offset, wrap, opacity: 1.0, over: false }]);
                }
            }
            current = out;
        }
        Some(Shot { picture: Picture::Texture(current), size: shown_size, premultiplied: shot.premultiplied })
    }
}

/// A texture of `size` to draw into and sample.
fn texture(device: &wgpu::Device, size: (u32, u32)) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("timed"),
        size: wgpu::Extent3d { width: size.0.max(1), height: size.1.max(1), depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

/// A spare texture of `size`, or a new one.
fn take(spare: &mut Vec<wgpu::Texture>, device: &wgpu::Device, size: (u32, u32)) -> wgpu::Texture {
    spare.retain(|t| (t.width(), t.height()) == size);
    spare.pop().unwrap_or_else(|| texture(device, size))
}

/// Draws `draws` into `target`, in order, cleared first.
fn run(gpu: &Gpu, pass: &Pass, target: &wgpu::Texture, draws: &[Draw]) {
    let device = gpu.device();
    let view = target.create_view(&Default::default());
    let groups: Vec<wgpu::BindGroup> = draws
        .iter()
        .map(|d| {
            let values = [d.offset.0, d.offset.1, if d.wrap { 1.0 } else { 0.0 }, d.opacity];
            let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("timed"),
                size: bytes.len() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            gpu.queue().write_buffer(&buffer, 0, &bytes);
            let from = d.from.create_view(&Default::default());
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("timed"),
                layout: &pass.layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&from) },
                    wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&pass.sampler) },
                ],
            })
        })
        .collect();
    let mut encoder = device.create_command_encoder(&Default::default());
    {
        let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("timed"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        for (d, group) in draws.iter().zip(&groups) {
            rp.set_pipeline(if d.over { &pass.over } else { &pass.copy });
            rp.set_bind_group(0, group, &[]);
            rp.draw(0..3, 0..1);
        }
    }
    gpu.queue().submit([encoder.finish()]);
}
