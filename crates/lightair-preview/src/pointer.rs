//! Lightair's own pointer and its follow-zoom, drawn by the compositor (macOS, with
//! `LIGHTAIR_ENGINE=own`) on the screens it captures: a scene's screen look, or a source's drawn
//! pointer effect, as libobs's pointer filter draws them (`lightair_obs::pointer`).
//!
//! - **One state, two renderers.** The pointer's state (followed on a spring, tilted, faded,
//!   its shape, the clicks, the follow-zoom's camera) is `lightair_host::pointer::Pointer`, which
//!   the libobs filter draws from too; each canvas's compositor keeps one per source it draws a
//!   pointer on, stepped every frame.
//! - **The pass.** The capture's picture is copied into a texture of the source's own, through
//!   the zoom's view when it's zoomed in (a crop scaled up), then the sprites (clicks, then the
//!   pointer) are drawn over it: Lightair's shapes (`lightair_render::shapes`, premultiplied, in
//!   the look's colour) or the system pointer's own picture, blended as premultiplied colour.
//! - **Only alone.** The compositor draws a pointer only on a source with no other effect
//!   (the look comes last, after the source's effects, as libobs puts it); a source with others
//!   is drawn by libobs, filters and all.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use lightair_engine::gpu::Gpu;
use lightair_host::bridge;
use lightair_host::pointer::{Art, Pointer, Style};
use lightair_model::{Id, Source, Workspace};
use lightair_render::media::Picture;
use lightair_render::scene::Shot;
use lightair_render::shapes::{RES, arrow, crosshair, disc, dot, ibeam, ring};

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// Each sprite's uniforms sit this far apart (the most any device asks for).
const SLOT: u64 = 256;
/// The most sprites drawn on one picture (clicks and the pointer).
const MOST: usize = 32;

const SHADER: &str = r#"
struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

struct Zoom { view: vec4<f32> };
@group(0) @binding(0) var<uniform> zoom: Zoom;
@group(0) @binding(1) var picture: texture_2d<f32>;
@group(0) @binding(2) var linear: sampler;

@vertex
fn vs_full(@builtin(vertex_index) i: u32) -> Out {
    let p = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: Out;
    o.pos = vec4<f32>(p * 2.0 - 1.0, 0.0, 1.0);
    o.uv = vec2<f32>(p.x, 1.0 - p.y);
    return o;
}

@fragment
fn fs_zoom(v: Out) -> @location(0) vec4<f32> {
    return textureSample(picture, linear, zoom.view.xy + v.uv * zoom.view.zw);
}

struct Sprite {
    at: vec2<f32>,
    size: f32,
    angle: f32,
    hot: vec2<f32>,
    canvas: vec2<f32>,
    opacity: f32,
    pad0: f32,
    pad1: f32,
    pad2: f32,
};
@group(0) @binding(0) var<uniform> sprite: Sprite;
@group(1) @binding(0) var art: texture_2d<f32>;
@group(1) @binding(1) var art_sampler: sampler;

@vertex
fn vs_sprite(@builtin(vertex_index) i: u32) -> Out {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), vec2<f32>(1.0, 0.0), vec2<f32>(0.0, 1.0),
        vec2<f32>(1.0, 0.0), vec2<f32>(1.0, 1.0), vec2<f32>(0.0, 1.0),
    );
    let c = corners[i];
    // pixels from the hot spot, turned about it (y down, as the picture's pixels are)
    let local = (c - sprite.hot) * sprite.size;
    let s = sin(sprite.angle);
    let k = cos(sprite.angle);
    let px = sprite.at + vec2<f32>(local.x * k - local.y * s, local.x * s + local.y * k);
    var o: Out;
    o.pos = vec4<f32>(px.x / sprite.canvas.x * 2.0 - 1.0, 1.0 - px.y / sprite.canvas.y * 2.0, 0.0, 1.0);
    o.uv = c;
    return o;
}

@fragment
fn fs_sprite(v: Out) -> @location(0) vec4<f32> {
    return textureSample(art, art_sampler, v.uv) * sprite.opacity;
}
"#;

/// The pipelines, made once.
struct Pass {
    zoom: wgpu::RenderPipeline,
    sprite: wgpu::RenderPipeline,
    zoom_layout: wgpu::BindGroupLayout,
    art_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// The zoom's view, and each sprite's uniforms (`SLOT` apart).
    zoom_uniform: wgpu::Buffer,
    sprite_uniforms: wgpu::Buffer,
    sprite_group: wgpu::BindGroup,
}

impl Pass {
    fn new(device: &wgpu::Device) -> Pass {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("pointer"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let uniform = |binding, dynamic| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: dynamic,
                min_binding_size: None,
            },
            count: None,
        };
        let texture = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Texture {
                sample_type: wgpu::TextureSampleType::Float { filterable: true },
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let sampler = |binding| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
            count: None,
        };
        let zoom_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pointer zoom"),
            entries: &[uniform(0, false), texture(1), sampler(2)],
        });
        let uniform_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pointer sprite"),
            entries: &[uniform(0, true)],
        });
        let art_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("pointer art"),
            entries: &[texture(0), sampler(1)],
        });
        let pipeline =
            |layouts: &[Option<&wgpu::BindGroupLayout>], vs: &str, fs: &str, blend: Option<wgpu::BlendState>| {
                let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("pointer"),
                    bind_group_layouts: layouts,
                    immediate_size: 0,
                });
                device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("pointer"),
                    layout: Some(&layout),
                    vertex: wgpu::VertexState {
                        module: &module,
                        entry_point: Some(vs),
                        compilation_options: Default::default(),
                        buffers: &[],
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &module,
                        entry_point: Some(fs),
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
        let zoom = pipeline(&[Some(&zoom_layout)], "vs_full", "fs_zoom", None);
        let sprite = pipeline(
            &[Some(&uniform_layout), Some(&art_layout)],
            "vs_sprite",
            "fs_sprite",
            Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
        );
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("pointer"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let buffer = |size| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("pointer"),
                size,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        };
        let zoom_uniform = buffer(16);
        let sprite_uniforms = buffer(SLOT * MOST as u64);
        let sprite_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pointer sprite"),
            layout: &uniform_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &sprite_uniforms,
                    offset: 0,
                    size: wgpu::BufferSize::new(48),
                }),
            }],
        });
        Pass { zoom, sprite, zoom_layout, art_layout, sampler, zoom_uniform, sprite_uniforms, sprite_group }
    }
}

/// A source's pointer in this compositor: its state, when it was last stepped, and the texture
/// its pictures are drawn into.
struct Drawn {
    pointer: Pointer,
    ticked: Instant,
    target: Option<wgpu::Texture>,
}

/// The pointers one compositor draws.
#[derive(Default)]
pub(crate) struct Pointers {
    pass: Option<Pass>,
    drawn: HashMap<Id, Drawn>,
    /// The art as textures, by what it is and (for the coloured ones) its colour.
    art: HashMap<(Art, u32), wgpu::BindGroup>,
    /// The system pointer's picture, by its serial.
    system: Option<(u64, wgpu::BindGroup)>,
}

/// The pointer each of `shown` draws here, when the compositor draws it: a pointer (a screen
/// look or a drawn pointer effect) on a source with no other effect.
pub(crate) fn styles(ws: &Workspace, shown: &HashSet<Id>) -> HashMap<Id, Style> {
    shown
        .iter()
        .filter_map(|id| ws.sources.get(id))
        .filter_map(|source| Some((source.id, lightair_host::pointer::style_for(ws, source)?)))
        .collect()
}

/// Whether `source` has an enabled picture effect other than the drawn pointer (then libobs
/// draws it, pointer and all).
pub(crate) fn has_other_effects(source: &Source) -> bool {
    source.effects.iter().any(|e| e.enabled && !e.kind.starts_with("audio.") && !is_pointer(e))
}

/// Whether `e` is the drawn pointer effect (the compositor draws it with the source's pointer).
pub(crate) fn is_pointer(e: &lightair_model::Effect) -> bool {
    e.kind == lightair_host::pointer::KIND || e.kind == lightair_model::catalog::OLD_POINTER_KIND
}

impl Pointers {
    /// Lets go of the pointers of sources not in `keep` (no longer shown, or drawn by libobs).
    pub(crate) fn keep(&mut self, keep: &HashMap<Id, Style>) {
        self.drawn.retain(|id, _| {
            let kept = keep.contains_key(id);
            if !kept {
                lightair_host::pointer::report_zoom(badge_key(*id), None);
            }
            kept
        });
    }

    /// `source`'s picture (`buffer`, one of Lightair's BGRA pictures) with its pointer drawn over
    /// it, zoomed as the pointer's camera is; `on_air`: the badge counts it. `None` when there's
    /// nothing to draw over it (not zoomed, no pointer showing) or the GPU couldn't: draw the
    /// picture as it is.
    pub(crate) fn draw(
        &mut self,
        gpu: &Gpu,
        source: &Source,
        style: Style,
        buffer: &bridge::Buffer,
        on_air: bool,
    ) -> Option<Shot> {
        let size = crate::buffer_gpu::size(buffer);
        let now = Instant::now();
        let drawn = self.drawn.entry(source.id).or_insert_with(|| Drawn {
            pointer: Pointer::new(style),
            ticked: now,
            target: None,
        });
        if *drawn.pointer.look() != style {
            drawn.pointer.set_look(style);
        }
        let dt = now.duration_since(drawn.ticked).as_secs_f32().max(1e-3);
        drawn.ticked = now;
        drawn.pointer.tick(dt, size, || lightair_host::pointer::frame_of(source));
        let zoom = drawn.pointer.zoom().filter(|_| on_air && style.badge());
        lightair_host::pointer::report_zoom(badge_key(source.id), zoom);
        let drawing = drawn.pointer.drawing(size);
        if drawing.view.is_none() && drawing.sprites.iter().all(|s| s.opacity <= 0.001) {
            return None;
        }
        let device = gpu.device();
        let queue = gpu.queue();
        let pass = self.pass.get_or_insert_with(|| Pass::new(device));
        let picture = crate::buffer_gpu::sampled(device, buffer)?;
        // the picture, zoomed or as it is, into the source's own texture
        if drawn.target.as_ref().is_none_or(|t| (t.width(), t.height()) != size) {
            drawn.target = Some(device.create_texture(&wgpu::TextureDescriptor {
                label: Some("pointer"),
                size: wgpu::Extent3d { width: size.0.max(1), height: size.1.max(1), depth_or_array_layers: 1 },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: FORMAT,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            }));
        }
        let target = drawn.target.clone()?;
        let view = drawing.view.unwrap_or((0.0, 0.0, 1.0, 1.0));
        queue.write_buffer(&pass.zoom_uniform, 0, &floats(&[view.0, view.1, view.2, view.3]));
        let picture_view = picture.create_view(&Default::default());
        let zoom_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("pointer zoom"),
            layout: &pass.zoom_layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: pass.zoom_uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&picture_view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&pass.sampler) },
            ],
        });
        // the sprites' uniforms and art
        let sprites: Vec<_> = drawing.sprites.iter().filter(|s| s.opacity > 0.001).take(MOST).collect();
        let mut uniforms = vec![0u8; SLOT as usize * sprites.len().max(1)];
        for (n, s) in sprites.iter().enumerate() {
            let at = n * SLOT as usize;
            let values = [
                s.at.0,
                s.at.1,
                s.size,
                s.angle.to_radians(),
                s.hot.0,
                s.hot.1,
                size.0 as f32,
                size.1 as f32,
                s.opacity,
                0.0,
                0.0,
                0.0,
            ];
            uniforms[at..at + 48].copy_from_slice(&floats(&values));
        }
        queue.write_buffer(&pass.sprite_uniforms, 0, &uniforms);
        let color = style.color();
        let mut groups = Vec::with_capacity(sprites.len());
        for s in &sprites {
            groups.push(match s.art {
                Art::System => {
                    let p = drawn.pointer.picture()?;
                    if self.system.as_ref().is_none_or(|(serial, _)| *serial != p.serial) {
                        let side = lightair_host::pointer::system_side();
                        let group = art_group(device, queue, pass, &p.rgba, side);
                        self.system = Some((p.serial, group));
                    }
                    self.system.as_ref().map(|(_, g)| g.clone())?
                }
                art => {
                    let key = (art, if coloured(art) { color } else { 0 });
                    self.art
                        .entry(key)
                        .or_insert_with(|| art_group(device, queue, pass, &bitmap(art, color), RES as u32))
                        .clone()
                }
            });
        }
        let target_view = target.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("pointer"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
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
            rp.set_pipeline(&pass.zoom);
            rp.set_bind_group(0, &zoom_group, &[]);
            rp.draw(0..3, 0..1);
            rp.set_pipeline(&pass.sprite);
            for (n, group) in groups.iter().enumerate() {
                rp.set_bind_group(0, &pass.sprite_group, &[(n as u64 * SLOT) as u32]);
                rp.set_bind_group(1, group, &[]);
                rp.draw(0..6, 0..1);
            }
        }
        queue.submit([encoder.finish()]);
        Some(Shot { picture: Picture::Texture(target), size, premultiplied: true })
    }
}

/// The key a compositor's pointer reports its zoom under, for the badge.
fn badge_key(source: Id) -> usize {
    // (filters report by address: an odd number never collides with one)
    (source.0.as_u128() as usize) | 1
}

/// Whether `art` is drawn in the look's colour.
fn coloured(art: Art) -> bool {
    matches!(art, Art::Dot | Art::Ring | Art::ClickDisc)
}

/// Lightair's picture for `art` (premultiplied RGBA, `RES` square), as the libobs filter makes it.
fn bitmap(art: Art, color: u32) -> Vec<u8> {
    let rgb =
        [(color & 0xFF) as f32 / 255.0, ((color >> 8) & 0xFF) as f32 / 255.0, ((color >> 16) & 0xFF) as f32 / 255.0];
    match art {
        Art::Arrow => arrow().rgba,
        Art::IBeam => ibeam().rgba,
        Art::Crosshair => crosshair().rgba,
        Art::Dot => dot(rgb).rgba,
        Art::Ring => ring(rgb, 0.36, 0.08).rgba,
        Art::ClickRing => ring([1.0, 1.0, 1.0], 0.44, 0.05).rgba,
        Art::ClickDisc => disc(rgb).rgba,
        Art::System => Vec::new(),
    }
}

/// A `side`-square RGBA picture as a texture, bound for the sprite pipeline.
fn art_group(device: &wgpu::Device, queue: &wgpu::Queue, pass: &Pass, rgba: &[u8], side: u32) -> wgpu::BindGroup {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("pointer art"),
        size: wgpu::Extent3d { width: side, height: side, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    if rgba.len() >= (side * side * 4) as usize {
        queue.write_texture(
            texture.as_image_copy(),
            &rgba[..(side * side * 4) as usize],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(side * 4), rows_per_image: Some(side) },
            wgpu::Extent3d { width: side, height: side, depth_or_array_layers: 1 },
        );
    }
    let view = texture.create_view(&Default::default());
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("pointer art"),
        layout: &pass.art_layout,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&pass.sampler) },
        ],
    })
}

fn floats(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}
