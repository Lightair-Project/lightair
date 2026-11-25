//! Effects that work across frames, drawn by the compositor on a source's picture before the
//! renderer's own effects (macOS, with `LIGHTAIR_ENGINE=own`):
//!
//! - **Freeze** (`lightair_freeze`): `frozen` holds the picture from the moment it's switched on
//!   (and again whenever the source is shown again); `hold_ms` thaws it after that long (0: until
//!   switched off); `fade` lets that much of the live picture through, under the frozen one.
//! - **Render delay** (`gpu_delay`): the picture `delay_ms` ago, from a ring of copies.
//! - **Scroll** (`scroll_filter`): the picture moving `speed_x`, `speed_y` pixels a second,
//!   wrapping round with `loop` (on by default), else moving off and leaving it transparent.
//! - **React to sound** (`lightair_audio_react`): with a track's level (`track`, from
//!   `floor_db` up), the picture grows (`react` 0), fades in (1), glows (2) or shakes (3), by
//!   `amount`.
//! - **Face blur** (`lightair_face_blur`): every face Vision finds, pixelated (`style` 0) or
//!   blurred (1) by `strength`, with `grow` around it.
//! - **Auto-frame** (`lightair_autoframe`): crops in on the biggest face at a shot size (`shot`
//!   0 close, 1 medium, 2 wide), no closer than `max_zoom`, easing there at `speed`.
//!
//! Faces are found with Apple's Vision on a small copy of the source's picture every third
//! frame, on a thread of each source's own; a picture made here (not a buffer) has none.
//!
//! They're drawn here only while they come before every other picture effect of the source (the
//! order libobs runs a source's filters in); otherwise libobs draws the source, filters and all.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use lightair_engine::gpu::Gpu;
use lightair_host::bridge;
use lightair_model::{Effect, Id, Source};
use lightair_render::media::Picture;
use lightair_render::scene::Shot;

pub(crate) const FREEZE: &str = "lightair_freeze";
pub(crate) const DELAY: &str = "gpu_delay";
pub(crate) const SCROLL: &str = "scroll_filter";
pub(crate) const REACT: &str = lightair_model::catalog::AUDIO_REACT_KIND;
pub(crate) const FACE_BLUR: &str = lightair_model::catalog::FACE_BLUR_KIND;
pub(crate) const AUTOFRAME: &str = lightair_model::catalog::AUTOFRAME_KIND;

/// The copy of the picture Vision looks at is at most this wide.
const WATCH_WIDTH: usize = 320;
/// The most faces blurred.
const FACES: usize = 6;

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// The most copies a delay keeps (half a second at 60 frames a second, and a little more).
const MOST_DELAYED: usize = 40;

const SHADER: &str = r#"
struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

struct P {
    offset: vec2<f32>, wrap: f32, opacity: f32, scale: f32, gain: f32, pad: vec2<f32>,
    // face blur: on (1), the block's size in pixels, pixelate (0) or blur (1)
    mode: f32, block: f32, style: f32, pad2: f32,
    texel: vec2<f32>, pad3: vec2<f32>,
    faces: array<vec4<f32>, 6>,
};
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

fn inside_of(uv: vec2<f32>, face: vec4<f32>) -> f32 {
    if (face.z <= 0.0) { return 0.0; }
    let c = face.xy + face.zw * 0.5;
    let d = (uv - c) / max(face.zw * 0.5, vec2<f32>(0.0001));
    return 1.0 - smoothstep(0.85, 1.0, length(d));
}

fn hidden(uv: vec2<f32>) -> vec4<f32> {
    let b = p.texel * p.block;
    if (p.style < 0.5) {
        return textureSampleLevel(picture, linear, (floor(uv / b) + 0.5) * b, 0.0);
    }
    var sum = vec4<f32>(0.0);
    for (var x = -3; x <= 3; x++) {
        for (var y = -3; y <= 3; y++) {
            sum += textureSampleLevel(picture, linear, uv + vec2<f32>(f32(x), f32(y)) * b * 0.5, 0.0);
        }
    }
    return sum / 49.0;
}

@fragment
fn fs(v: Out) -> @location(0) vec4<f32> {
    if (p.mode > 0.5) {
        var inside = 0.0;
        for (var i = 0; i < 6; i++) {
            inside = max(inside, inside_of(v.uv, p.faces[i]));
        }
        let orig = textureSampleLevel(picture, linear, v.uv, 0.0);
        if (inside <= 0.0) { return orig; }
        return mix(orig, hidden(v.uv), inside);
    }
    var uv = (v.uv - vec2<f32>(0.5)) / p.scale + vec2<f32>(0.5) + p.offset;
    if (p.wrap > 0.5) {
        uv = fract(uv);
    } else if (uv.x < 0.0 || uv.y < 0.0 || uv.x > 1.0 || uv.y > 1.0) {
        return vec4<f32>(0.0);
    }
    let c = textureSampleLevel(picture, linear, uv, 0.0);
    return vec4<f32>(min(c.rgb * p.gain, vec3<f32>(c.a)), c.a) * p.opacity;
}
"#;

/// Whether `e` is one of the effects drawn here.
pub(crate) fn is_timed(e: &Effect) -> bool {
    matches!(e.kind.as_str(), FREEZE | DELAY | SCROLL | REACT | FACE_BLUR | AUTOFRAME)
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
    /// Bigger about its middle (1: as it is), and brighter (1: as it is).
    scale: f32,
    gain: f32,
    /// Face blur: the faces (x, y, w, h, 0..1; none: nothing hidden), the block's size in
    /// pixels and the style (0 pixelate, 1 blur), and one pixel in uv.
    faces: Option<[[f32; 4]; FACES]>,
    block: f32,
    style: f32,
    texel: (f32, f32),
}

impl<'a> Draw<'a> {
    /// `from` as it is.
    fn copy(from: &'a wgpu::Texture) -> Draw<'a> {
        Draw {
            from,
            offset: (0.0, 0.0),
            wrap: false,
            opacity: 1.0,
            over: false,
            scale: 1.0,
            gain: 1.0,
            faces: None,
            block: 1.0,
            style: 0.0,
            texel: (0.0, 0.0),
        }
    }
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
    /// React to sound: the level as it moves (0..1: up at once, down slowly), when it last
    /// moved, and the shake's random numbers.
    level: f32,
    reacted: Option<Instant>,
    seed: u32,
    /// The faces seen (Face blur, Auto-frame), and Auto-frame's view as it moves (x, y, w, h
    /// in 0..1) and when it last moved.
    watch: Option<Watch>,
    view: Option<[f32; 4]>,
    framed: Option<Instant>,
}

/// Every face in a source's picture, found by Apple's Vision on a thread of its own from a
/// small copy of every third picture.
struct Watch {
    to: std::sync::mpsc::SyncSender<(Vec<u8>, usize, usize)>,
    /// The faces found last (x, y, w, h, 0..1 from the top left), and when.
    found: std::sync::Arc<parking_lot::Mutex<(Vec<[f32; 4]>, Instant)>>,
    frames: u32,
}

impl Watch {
    fn new() -> Option<Watch> {
        let (to, from) = std::sync::mpsc::sync_channel::<(Vec<u8>, usize, usize)>(1);
        let found = std::sync::Arc::new(parking_lot::Mutex::new((Vec::new(), Instant::now())));
        let into = found.clone();
        std::thread::Builder::new()
            .name("lightair-faces".into())
            .spawn(move || {
                while let Ok((mut bytes, w, h)) = from.recv() {
                    let faces = lightair_host::vision::faces(&mut bytes, w, h, w * 4);
                    *into.lock() = (faces, Instant::now());
                }
            })
            .ok()?;
        Some(Watch { to, found, frames: 0 })
    }

    /// Hands Vision a small copy of `buffer` (every third time asked; dropped while it's busy).
    fn look(&mut self, buffer: &bridge::Buffer) {
        use objc2_core_video::{
            CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight, CVPixelBufferGetWidth,
            CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
        };
        self.frames += 1;
        if !self.frames.is_multiple_of(3) {
            return;
        }
        let (w, h) = (CVPixelBufferGetWidth(buffer), CVPixelBufferGetHeight(buffer));
        if w == 0 || h == 0 {
            return;
        }
        let step = w.div_ceil(WATCH_WIDTH).max(1);
        let (sw, sh) = (w / step, h / step);
        let mut small = Vec::with_capacity(sw * sh * 4);
        // SAFETY: the buffer's bytes are read only while it's locked, within its rows
        unsafe {
            if CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly) != 0 {
                return;
            }
            let base = CVPixelBufferGetBaseAddress(buffer) as *const u8;
            let stride = CVPixelBufferGetBytesPerRow(buffer);
            if !base.is_null() {
                for y in 0..sh {
                    let row = std::slice::from_raw_parts(base.add(y * step * stride), w * 4);
                    for x in 0..sw {
                        small.extend_from_slice(&row[x * step * 4..x * step * 4 + 4]);
                    }
                }
            }
            CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly);
        }
        if small.len() == sw * sh * 4 {
            let _ = self.to.try_send((small, sw, sh));
        }
    }

    /// The faces seen lately (none after a moment without any).
    fn faces(&self) -> Vec<[f32; 4]> {
        let f = self.found.lock();
        if f.1.elapsed() < Duration::from_millis(700) { f.0.clone() } else { Vec::new() }
    }
}

/// The view that frames `face` (a box in the picture) at `shot`, kept inside the picture.
fn framing(face: Option<[f32; 4]>, shot: i64, max_zoom: f32) -> [f32; 4] {
    let Some([fx, fy, fw, fh]) = face else { return [0.0, 0.0, 1.0, 1.0] };
    // how much of the frame's height the face fills
    let fill = match shot {
        0 => 0.45,
        2 => 0.16,
        _ => 0.28,
    };
    let zoom = (fill / fh.max(0.01)).clamp(1.0, max_zoom);
    let (vw, vh) = (1.0 / zoom, 1.0 / zoom);
    // the face a little above the middle (headroom)
    let (cx, cy) = (fx + fw * 0.5, fy + fh * 0.5 + vh * 0.08);
    let x = (cx - vw / 2.0).clamp(0.0, 1.0 - vw);
    let y = (cy - vh / 2.0).clamp(0.0, 1.0 - vh);
    [x, y, vw, vh]
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
                        run(gpu, pass, &held, &[Draw::copy(&current)]);
                        state.frozen = Some((held, now));
                    }
                    let (held, since) = state.frozen.as_ref()?;
                    if hold > 0 && now.duration_since(*since) >= Duration::from_millis(hold) {
                        // thawed: the live picture
                        continue;
                    }
                    let mut draws = Vec::new();
                    if fade > 0.0 {
                        draws.push(Draw::copy(&current));
                    }
                    draws.push(Draw { opacity: 1.0 - fade, over: fade > 0.0, ..Draw::copy(held) });
                    run(gpu, pass, &out, &draws);
                }
                DELAY => {
                    let delay = Duration::from_millis(num("delay_ms", 0.0).clamp(0.0, 500.0) as u64);
                    // this frame's picture into the ring
                    let copy = take(&mut state.spare, device, size);
                    run(gpu, pass, &copy, &[Draw::copy(&current)]);
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
                    run(gpu, pass, &out, &[Draw::copy(shown)]);
                }
                SCROLL => {
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
                    run(gpu, pass, &out, &[Draw { offset, wrap, ..Draw::copy(&current) }]);
                }
                REACT => {
                    let track = e.settings.get("track").and_then(serde_json::Value::as_str).unwrap_or("");
                    let floor = num("floor_db", -40.0) as f32;
                    let floor = if floor < -1.0 { floor } else { -40.0 };
                    let amount = num("amount", 0.5).clamp(0.0, 1.0) as f32;
                    let db = serde_json::from_value::<Id>(serde_json::Value::String(track.to_string()))
                        .ok()
                        .and_then(|t| lightair_host::meters::meters().get(t))
                        .map(|c| {
                            let l = c.levels();
                            if l.age > 0.5 {
                                f32::NEG_INFINITY
                            } else {
                                l.peak.iter().copied().fold(f32::NEG_INFINITY, f32::max)
                            }
                        })
                        .unwrap_or(f32::NEG_INFINITY);
                    let target = if db.is_finite() { ((db - floor) / -floor).clamp(0.0, 1.0) } else { 0.0 };
                    // up at once, down over about a fifth of a second
                    let dt = state.reacted.map_or(0.0, |t| now.duration_since(t).as_secs_f32());
                    state.reacted = Some(now);
                    state.level = if target > state.level {
                        target
                    } else {
                        state.level + (target - state.level) * (dt * 6.0).min(1.0)
                    };
                    let k = state.level * amount;
                    let draw = match num("react", 0.0) as i64 {
                        // grows from its middle
                        0 => Draw { scale: 1.0 + 0.35 * k, ..Draw::copy(&current) },
                        1 => Draw { opacity: ((1.0 - amount) + k).clamp(0.0, 1.0), ..Draw::copy(&current) },
                        2 => Draw { gain: 1.0 + 1.5 * k, ..Draw::copy(&current) },
                        _ => {
                            state.seed = state.seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
                            let a = (state.seed >> 16) as f32 / 65_536.0;
                            let b = (state.seed & 0xffff) as f32 / 65_536.0;
                            let r = 0.03 * size.0.min(size.1) as f32 * k;
                            let px = ((a - 0.5) * 2.0 * r, (b - 0.5) * 2.0 * r);
                            Draw {
                                offset: (-px.0 / size.0.max(1) as f32, -px.1 / size.1.max(1) as f32),
                                ..Draw::copy(&current)
                            }
                        }
                    };
                    run(gpu, pass, &out, &[draw]);
                }
                FACE_BLUR | AUTOFRAME => {
                    if state.watch.is_none() {
                        state.watch = Watch::new();
                    }
                    let Some(watch) = state.watch.as_mut() else { continue };
                    if let Some(b) = buffer {
                        watch.look(b);
                    }
                    let faces = watch.faces();
                    if e.kind == FACE_BLUR {
                        // each face's box, grown (hair, ears, chin)
                        let g = 1.0 + num("grow", 0.3).clamp(0.0, 1.0) as f32;
                        let strength = num("strength", 0.6).clamp(0.0, 1.0) as f32;
                        let mut boxes = [[0.0f32; 4]; FACES];
                        for (b, [x, y, fw, fh]) in boxes.iter_mut().zip(&faces) {
                            let (cx, cy) = (x + fw / 2.0, y + fh / 2.0);
                            let (gw, gh) = (fw * g * 1.1, fh * g * 1.25);
                            *b = [cx - gw / 2.0, cy - gh / 2.0, gw, gh];
                        }
                        let draw = Draw {
                            faces: Some(boxes),
                            block: 4.0 + strength * (size.0.min(size.1) as f32 / 18.0),
                            style: num("style", 0.0) as f32,
                            texel: (1.0 / size.0.max(1) as f32, 1.0 / size.1.max(1) as f32),
                            ..Draw::copy(&current)
                        };
                        run(gpu, pass, &out, &[draw]);
                    } else {
                        // the biggest face (the person in front)
                        let face = faces.into_iter().max_by(|a, b| (a[2] * a[3]).total_cmp(&(b[2] * b[3])));
                        let want = framing(face, num("shot", 1.0) as i64, num("max_zoom", 2.5).clamp(1.0, 4.0) as f32);
                        let speed = num("speed", 0.4).clamp(0.05, 1.0) as f32;
                        let dt = state.framed.map_or(0.0, |t| now.duration_since(t).as_secs_f32());
                        state.framed = Some(now);
                        let view = state.view.get_or_insert([0.0, 0.0, 1.0, 1.0]);
                        // eases there; small moves inside a dead zone are left alone (no wobble)
                        let k = (dt * (0.6 + 4.0 * speed)).min(1.0);
                        let far = (0..4).map(|i| (want[i] - view[i]).abs()).fold(0.0f32, f32::max);
                        if far > 0.015 {
                            for (v, w) in view.iter_mut().zip(want) {
                                *v += (w - *v) * k;
                            }
                        }
                        let [x, y, w, h] = *view;
                        // the view's part of the picture, filling it
                        let draw = Draw {
                            scale: 1.0 / w.max(0.01),
                            offset: (x - 0.5 + 0.5 * w, y - 0.5 + 0.5 * h),
                            ..Draw::copy(&current)
                        };
                        run(gpu, pass, &out, &[draw]);
                    }
                }
                _ => continue,
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
            let mut values = vec![
                d.offset.0,
                d.offset.1,
                if d.wrap { 1.0 } else { 0.0 },
                d.opacity,
                d.scale,
                d.gain,
                0.0,
                0.0,
                if d.faces.is_some() { 1.0 } else { 0.0 },
                d.block,
                d.style,
                0.0,
                d.texel.0,
                d.texel.1,
                0.0,
                0.0,
            ];
            values.extend(d.faces.unwrap_or_default().iter().flatten());
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
