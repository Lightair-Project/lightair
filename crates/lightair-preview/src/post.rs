//! The camera looks' post pass, drawn by the compositor (`crate::timed` runs it in a source's
//! effect order): **Camera motion** (`studio.motion`: `motion_preset` — Handheld, Beat Pulse,
//! RGB Split, Slow Drift, Glitch, Wobble — and `motion_amt`, 0..100) and **Ambience**
//! (`studio.ambience`: `atmo_preset` — sparkles, bokeh, hearts, petals, snow, fireflies, light
//! leaks, mist, glitter, dreamy, confetti, bubbles — and `atmo_amt`, 0..100). The presets are the
//! Effect Stack's (`atmosphere.json`); the pictures are procedural, as its post pass draws them.

use std::sync::OnceLock;
use std::time::Instant;

use lightair_engine::gpu::Gpu;
use serde_json::Value;

pub(crate) const MOTION: &str = "studio.motion";
pub(crate) const AMBIENCE: &str = "studio.ambience";

const PRESETS: &str = include_str!("../../../modules/lightair-effect-stack/data/atmosphere.json");
const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

const ATMO_STYLES: [&str; 13] = [
    "none",
    "sparkle",
    "bokeh",
    "hearts",
    "petals",
    "snow",
    "fireflies",
    "leak",
    "mist",
    "glitter",
    "dreamy",
    "confetti",
    "bubbles",
];
const MOTION_STYLES: [&str; 7] = ["none", "shake", "pulse", "rgb", "drift", "glitch", "wobble"];

/// What one draw of the pass does.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Params {
    motion: (u32, f32, f32),
    atmo: (u32, f32),
    colors: [[f32; 3]; 2],
    /// Density, speed, size.
    atmo_params: [f32; 3],
}

fn presets() -> &'static Value {
    static ALL: OnceLock<Value> = OnceLock::new();
    ALL.get_or_init(|| serde_json::from_str(PRESETS).unwrap_or(Value::Null))
}

fn find<'a>(list: &str, name: &str) -> Option<&'a Value> {
    presets()[list].as_array()?.iter().find(|p| p["name"].as_str() == Some(name))
}

/// "#ffe2a8" as 0..1 (sRGB, as the canvas draws it).
fn hex(v: &Value) -> [f32; 3] {
    let s = v.as_str().unwrap_or("").trim_start_matches('#');
    let n = u32::from_str_radix(s, 16).unwrap_or(0xFFFFFF);
    [((n >> 16) & 0xFF) as f32 / 255.0, ((n >> 8) & 0xFF) as f32 / 255.0, (n & 0xFF) as f32 / 255.0]
}

impl Params {
    /// Camera motion's, from its effect's settings (`None`: nothing to draw).
    pub(crate) fn motion(s: &Value) -> Option<Params> {
        let amt = (s.get("motion_amt").and_then(Value::as_f64).unwrap_or(50.0) / 100.0).clamp(0.0, 1.0) as f32;
        let p = find("motion", s.get("motion_preset").and_then(Value::as_str).unwrap_or("Handheld"))?;
        let style = MOTION_STYLES.iter().position(|n| p["style"].as_str() == Some(n)).unwrap_or(0) as u32;
        let speed = p["speed"].as_f64().unwrap_or(1.0) as f32;
        (style > 0 && amt > 0.0).then_some(Params { motion: (style, amt, speed), ..Default::default() })
    }

    /// Ambience's, from its effect's settings (`None`: nothing to draw).
    pub(crate) fn ambience(s: &Value) -> Option<Params> {
        let amt = (s.get("atmo_amt").and_then(Value::as_f64).unwrap_or(70.0) / 100.0).clamp(0.0, 1.0) as f32;
        let p = find("ambience", s.get("atmo_preset").and_then(Value::as_str).unwrap_or("Sparkles"))?;
        let style = ATMO_STYLES.iter().position(|n| p["style"].as_str() == Some(n)).unwrap_or(0) as u32;
        let f = |k: &str| p[k].as_f64().unwrap_or(1.0) as f32;
        (style > 0 && amt > 0.0).then_some(Params {
            atmo: (style, amt),
            colors: [hex(&p["a"]), hex(&p["b"])],
            atmo_params: [f("density"), f("speed"), f("size")],
            ..Default::default()
        })
    }
}

/// The pass's pipeline, made once.
pub(crate) struct Post {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
}

impl Post {
    pub(crate) fn new(device: &wgpu::Device) -> Post {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("looks post"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty,
            count: None,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("looks post"),
            entries: &[
                entry(
                    0,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                entry(
                    1,
                    wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                ),
                entry(2, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("looks post"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("looks post"),
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
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("looks post"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        Post { pipeline, layout, sampler }
    }

    /// Draws `from` into `to` (the same size) through `p`, at the show's time now.
    pub(crate) fn draw(&self, gpu: &Gpu, from: &wgpu::Texture, to: &wgpu::Texture, p: &Params) {
        static START: OnceLock<Instant> = OnceLock::new();
        let t = START.get_or_init(Instant::now).elapsed().as_secs_f32();
        let device = gpu.device();
        let values: [f32; 20] = [
            to.width() as f32,
            to.height() as f32,
            t,
            p.motion.0 as f32,
            p.motion.1,
            p.motion.2,
            p.atmo.0 as f32,
            p.atmo.1,
            p.colors[0][0],
            p.colors[0][1],
            p.colors[0][2],
            1.0,
            p.colors[1][0],
            p.colors[1][1],
            p.colors[1][2],
            1.0,
            p.atmo_params[0],
            p.atmo_params[1],
            p.atmo_params[2],
            0.0,
        ];
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("looks post"),
            size: bytes.len() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue().write_buffer(&buffer, 0, &bytes);
        let from_view = from.create_view(&Default::default());
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("looks post"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&from_view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        });
        let view = to.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("looks post"),
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
            rp.set_pipeline(&self.pipeline);
            rp.set_bind_group(0, &group, &[]);
            rp.draw(0..3, 0..1);
        }
        gpu.queue().submit([encoder.finish()]);
    }
}

const SHADER: &str = r#"
struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

struct Q {
    size: vec2<f32>,
    time: f32,
    motion_style: f32,
    motion_amt: f32,
    motion_speed: f32,
    atmo_style: f32,
    atmo_amt: f32,
    color_a: vec4<f32>,
    color_b: vec4<f32>,
    atmo_params: vec4<f32>,
};
@group(0) @binding(0) var<uniform> q: Q;
@group(0) @binding(1) var image: texture_2d<f32>;
@group(0) @binding(2) var linear: sampler;

@vertex
fn vs(@builtin(vertex_index) i: u32) -> Out {
    let c = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: Out;
    o.pos = vec4<f32>(c * 2.0 - 1.0, 0.0, 1.0);
    o.uv = vec2<f32>(c.x, 1.0 - c.y);
    return o;
}

fn sample(uv: vec2<f32>) -> vec4<f32> {
    return textureSampleLevel(image, linear, clamp(uv, vec2<f32>(0.0), vec2<f32>(1.0)), 0.0);
}

// smoothstep with either order of edges (as HLSL's)
fn ss(e0: f32, e1: f32, x: f32) -> f32 {
    let t = clamp((x - e0) / (e1 - e0), 0.0, 1.0);
    return t * t * (3.0 - 2.0 * t);
}

fn hash21(p0: vec2<f32>) -> f32 {
    var p = fract(p0 * vec2<f32>(123.34, 456.21));
    p += dot(p, p + 45.32);
    return fract(p.x * p.y);
}

fn hash22(p: vec2<f32>) -> vec2<f32> {
    let n = hash21(p);
    return vec2<f32>(n, hash21(p + n + 17.17));
}

fn vnoise(p: vec2<f32>) -> f32 {
    let i = floor(p);
    let f = fract(p);
    let u = f * f * (3.0 - 2.0 * f);
    let a = hash21(i);
    let b = hash21(i + vec2<f32>(1.0, 0.0));
    let c = hash21(i + vec2<f32>(0.0, 1.0));
    let d = hash21(i + vec2<f32>(1.0, 1.0));
    return mix(mix(a, b, u.x), mix(c, d, u.x), u.y);
}

fn fbm(p0: vec2<f32>) -> f32 {
    var p = p0;
    var s = 0.0;
    var a = 0.5;
    for (var i = 0; i < 4; i++) {
        s += vnoise(p) * a;
        p = p * 2.03 + vec2<f32>(1.7, 9.2);
        a *= 0.5;
    }
    return s;
}

fn rot2(v: vec2<f32>, a: f32) -> vec2<f32> {
    let c = cos(a);
    let s = sin(a);
    return vec2<f32>(v.x * c - v.y * s, v.x * s + v.y * c);
}

// heart SDF (negative inside), in units of the heart size, y up
fn sd_heart(q0: vec2<f32>) -> f32 {
    let q = vec2<f32>(abs(q0.x), q0.y);
    if (q.y + q.x > 1.0) {
        let d = q - vec2<f32>(0.25, 0.75);
        return sqrt(dot(d, d)) - 0.35355;
    }
    let d1 = q - vec2<f32>(0.0, 1.0);
    let m = 0.5 * max(q.x + q.y, 0.0);
    let d2 = q - vec2<f32>(m, m);
    return sqrt(min(dot(d1, d1), dot(d2, d2))) * sign(q.x - q.y);
}

// one particle a grid cell (some empty), the grid moving over time; the 3x3 neighbourhood so
// particles cross cell borders. Premultiplied rgb, and coverage in a.
fn particle_layer(p: vec2<f32>, cell: f32, vel: vec2<f32>, seed: f32, density: f32, t: f32) -> vec4<f32> {
    let style = i32(q.atmo_style + 0.5);
    var acc = vec4<f32>(0.0);
    let moved = p - vel * t;
    let g = floor(moved / cell);
    for (var oy = -1; oy <= 1; oy++) {
        for (var ox = -1; ox <= 1; ox++) {
            let id = g + vec2<f32>(f32(ox), f32(oy));
            let r = hash22(id + seed);
            let present = step(hash21(id * 1.31 + seed + 3.7), density);
            if (present > 0.0) {
                let center = (id + 0.2 + 0.6 * r) * cell;
                let ph = hash21(id + seed + 9.1) * 6.2831;
                let sz = cell * (0.18 + 0.22 * hash21(id + seed + 5.3)) * q.atmo_params.z;
                let d0 = moved - center;
                var a = 0.0;
                var col = mix(q.color_a.rgb, q.color_b.rgb, hash21(id + seed + 2.2));
                if (style == 1 || style == 9) {
                    // sparkle / glitter: four-point stars
                    var tw = 0.5 + 0.5 * sin(t * 3.0 * q.atmo_params.y + ph);
                    tw = tw * tw;
                    var s2 = sz * 0.9;
                    if (style == 9) { s2 = sz * 0.35; }
                    let aq = abs(d0) / s2;
                    let star = max(0.0, 1.0 - aq.x * 7.0) * max(0.0, 1.0 - aq.y) + max(0.0, 1.0 - aq.y * 7.0) * max(0.0, 1.0 - aq.x);
                    let core = exp(-dot(d0, d0) / (s2 * s2 * 0.02));
                    a = saturate((star + core) * tw);
                } else if (style == 2) {
                    // bokeh
                    let d = length(d0) / (sz * 1.6);
                    a = (ss(1.0, 0.85, d) * 0.35 + ss(1.0, 0.93, d) * ss(0.85, 0.95, d) * 0.3) * (0.6 + 0.4 * sin(t * 0.8 + ph));
                } else if (style == 3) {
                    // hearts
                    let hq = rot2(d0 + vec2<f32>(sin(t * 1.3 + ph) * cell * 0.15, 0.0), sin(t + ph) * 0.4);
                    let d = sd_heart(vec2<f32>(hq.x, -hq.y) / sz + vec2<f32>(0.0, 0.5));
                    a = ss(0.04, -0.04, d);
                    col = col * (0.85 + 0.3 * saturate(-d * 3.0));
                } else if (style == 4) {
                    // petals: a notched teardrop that tumbles
                    var pq = rot2(d0 + vec2<f32>(sin(t * 0.9 + ph) * cell * 0.25, 0.0), t * 1.1 + ph);
                    pq.x = pq.x / (0.5 + 0.35 * abs(sin(t * 1.7 + ph * 2.0)));
                    let u2 = pq / (sz * 0.7);
                    let d = length(u2 * vec2<f32>(1.0, 0.75 + 0.25 * ss(-1.0, 1.0, u2.y)));
                    let notch = ss(0.18, 0.0, length(u2 - vec2<f32>(0.0, -0.95)));
                    a = ss(1.0, 0.82, d) * (1.0 - notch) * 0.9;
                    col = mix(col, vec3<f32>(1.0, 0.97, 0.98), 0.45 * ss(0.8, 0.0, d));
                } else if (style == 5) {
                    // snow
                    let sq = d0 + vec2<f32>(sin(t * 0.7 + ph) * cell * 0.2, 0.0);
                    let d = length(sq) / (sz * 0.45);
                    a = ss(1.0, 0.3, d) * 0.9;
                } else if (style == 6) {
                    // fireflies
                    let fq = d0 + vec2<f32>(sin(t * 0.6 + ph), cos(t * 0.5 + ph * 1.3)) * cell * 0.25;
                    let d2 = dot(fq, fq) / (sz * sz * 0.08);
                    a = (exp(-d2) * 1.2 + exp(-d2 * 0.15) * 0.25) * (0.5 + 0.5 * sin(t * 2.0 + ph));
                } else if (style == 11) {
                    // confetti
                    var cq = rot2(d0, t * 2.0 * (0.5 + r.x) + ph);
                    cq.y = cq.y * (1.0 + 0.6 * sin(t * 4.0 + ph));
                    let aq = abs(cq) / vec2<f32>(sz * 0.45, sz * 0.22);
                    a = ss(1.0, 0.85, max(aq.x, aq.y));
                    let pal = mix(vec3<f32>(1.0, 0.85, 0.2), vec3<f32>(0.3, 0.75, 1.0), step(0.5, r.y));
                    col = mix(col, pal, step(0.66, hash21(id + 8.8)));
                } else if (style == 12) {
                    // bubbles
                    let d = length(d0) / (sz * 1.2);
                    let ring = ss(1.0, 0.92, d) * ss(0.75, 0.95, d);
                    let hl = d0 / (sz * 1.2) - vec2<f32>(-0.35, -0.35);
                    a = ring * 0.8 + ss(0.25, 0.0, length(hl)) * 0.7 + ss(1.0, 0.0, d) * 0.06;
                }
                a = saturate(a);
                acc = vec4<f32>(acc.rgb * (1.0 - a) + col * a, acc.a + a * (1.0 - acc.a));
            }
        }
    }
    return acc;
}

fn atmosphere(uv: vec2<f32>, t: f32) -> vec4<f32> {
    let aspect = q.size.x / q.size.y;
    let p = vec2<f32>(uv.x * aspect, uv.y);
    var dens = saturate(0.45 * q.atmo_params.x);
    let sp = q.atmo_params.y;
    let s = i32(q.atmo_style + 0.5);
    if (s == 7) {
        // light leaks: warm gradients drifting along the edges
        let c1 = vec2<f32>(0.1 + 0.15 * sin(t * 0.21 * sp), 0.2 + 0.3 * sin(t * 0.13 * sp + 1.0));
        let c2 = vec2<f32>(aspect - 0.15 + 0.1 * sin(t * 0.17 * sp + 2.0), 0.8 + 0.2 * sin(t * 0.11 * sp));
        let g1 = exp(-dot(p - c1, p - c1) * 3.0);
        let g2 = exp(-dot(p - c2, p - c2) * 2.5);
        return vec4<f32>(q.color_a.rgb * g1 * 0.8 + q.color_b.rgb * g2 * 0.7, 0.0);
    }
    if (s == 8) {
        // mist: drifting fog hugging the bottom of the frame
        let n = fbm(vec2<f32>(p.x * 2.5 + t * 0.12 * sp, p.y * 3.0 - t * 0.05 * sp));
        let n2 = fbm(vec2<f32>(p.x * 5.0 - t * 0.2 * sp, p.y * 6.0));
        let h = ss(0.35, 1.0, uv.y);
        let a = saturate((n * 0.8 + n2 * 0.4 - 0.25) * 1.6) * h * 0.9;
        let col = mix(q.color_a.rgb, q.color_b.rgb, n2);
        return vec4<f32>(col * a, -a);
    }
    if (s == 10) {
        // dreamy: a soft glow from the picture's own highlights, a tinted haze, a vignette
        var glow = vec3<f32>(0.0);
        let px = 1.0 / q.size;
        for (var i = 0; i < 12; i++) {
            let an = f32(i) * 0.5236;
            let o = vec2<f32>(cos(an), sin(an)) * px * (14.0 + 10.0 * f32(i % 2));
            let c = sample(uv + o).rgb;
            glow += max(c - 0.55, vec3<f32>(0.0));
        }
        glow = glow / 12.0 * 2.2;
        let vig = ss(0.35, 1.0, length((uv - 0.5) * vec2<f32>(aspect, 1.0)) / (aspect * 0.6));
        return vec4<f32>(glow * q.color_a.rgb + q.color_b.rgb * 0.10 + q.color_b.rgb * vig * 0.25, 0.0);
    }
    var vel = vec2<f32>(0.0, 0.08) * sp;
    if (s == 3 || s == 12 || s == 2) { vel = vec2<f32>(0.0, -0.06) * sp; }
    if (s == 1 || s == 9 || s == 6) { vel = vec2<f32>(0.01, 0.0) * sp; }
    var cell_a = 0.13;
    if (s == 9) { cell_a = 0.05; } else if (s == 2) { cell_a = 0.22; }
    if (s == 4 || s == 3 || s == 11) { dens = dens * 0.45; }
    var far = particle_layer(p, cell_a * 0.7, vel * 0.6, 11.0, dens * 0.8, t);
    let near = particle_layer(p, cell_a, vel, 37.0, dens, t);
    far = vec4<f32>(far.rgb * 0.7, far.a * 0.7);
    let acc = vec4<f32>(far.rgb * (1.0 - near.a) + near.rgb, far.a + near.a * (1.0 - far.a));
    if (s == 1 || s == 2 || s == 6 || s == 9 || s == 12) {
        return vec4<f32>(acc.rgb, 0.0);
    }
    return vec4<f32>(acc.rgb, -acc.a);
}

fn motion_uv(uv: vec2<f32>, t: f32, amt: f32) -> vec2<f32> {
    let aspect = q.size.x / q.size.y;
    let sp = q.motion_speed;
    let style = i32(q.motion_style + 0.5);
    if (style == 1) {
        // handheld shake
        let o = vec2<f32>(sin(t * 13.0 * sp) * 0.6 + sin(t * 7.3 * sp + 1.3), cos(t * 11.0 * sp) * 0.6 + sin(t * 5.1 * sp + 4.0)) * 0.006 * amt;
        let r = sin(t * 4.7 * sp) * 0.006 * amt;
        var c = (uv - 0.5) * vec2<f32>(aspect, 1.0);
        c = rot2(c, r) * (1.0 - 0.025 * amt);
        return c / vec2<f32>(aspect, 1.0) + 0.5 + o;
    }
    if (style == 2) {
        // beat pulse
        let beat = exp(-fract(t * sp * 2.0) * 7.0);
        return (uv - 0.5) * (1.0 - 0.07 * amt * beat) + 0.5;
    }
    if (style == 4) {
        // slow drift
        let z = 1.0 - amt * (0.04 + 0.03 * sin(t * 0.25 * sp));
        let pan = vec2<f32>(sin(t * 0.17 * sp), cos(t * 0.13 * sp)) * 0.015 * amt;
        return (uv - 0.5) * z + 0.5 + pan;
    }
    if (style == 5) {
        // glitch: horizontal bands jump
        let slot = floor(t * 10.0 * sp);
        let on = step(0.72, hash21(vec2<f32>(slot, 3.0)));
        let band = floor(uv.y * 18.0);
        let shift = (hash21(vec2<f32>(band, slot)) - 0.5) * 0.08 * amt * on * step(0.6, hash21(vec2<f32>(band, slot + 1.0)));
        return uv + vec2<f32>(shift, 0.0);
    }
    if (style == 6) {
        // wobble
        return uv + vec2<f32>(sin(uv.y * 10.0 + t * 4.0 * sp), cos(uv.x * 8.0 + t * 3.0 * sp)) * 0.004 * amt;
    }
    return uv;
}

@fragment
fn fs(v: Out) -> @location(0) vec4<f32> {
    let t = q.time;
    let uv = v.uv;
    let style = i32(q.motion_style + 0.5);
    var col: vec4<f32>;
    if (style > 0) {
        let m = motion_uv(uv, t, q.motion_amt);
        var split = 0.0;
        if (style == 3) {
            split = 0.006 * q.motion_amt * (0.6 + 0.4 * sin(t * 3.0 * q.motion_speed));
        }
        if (style == 5) {
            split = 0.01 * q.motion_amt * step(0.72, hash21(vec2<f32>(floor(t * 10.0 * q.motion_speed), 3.0)));
        }
        col = sample(m);
        if (split > 0.0) {
            col.r = sample(m + vec2<f32>(split, 0.0)).r;
            col.b = sample(m - vec2<f32>(split, 0.0)).b;
        }
    } else {
        col = sample(uv);
    }
    if (q.atmo_style > 0.5 && q.atmo_amt > 0.0) {
        let a = atmosphere(uv, t);
        if (a.a < 0.0) {
            // covering
            col = vec4<f32>(col.rgb * (1.0 + a.a * q.atmo_amt) + a.rgb * q.atmo_amt, col.a);
        } else {
            // light added
            col = vec4<f32>(col.rgb + a.rgb * q.atmo_amt, col.a);
        }
    }
    return vec4<f32>(saturate(col.rgb), col.a);
}
"#;
