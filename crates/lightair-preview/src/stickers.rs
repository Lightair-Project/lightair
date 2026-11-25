//! The camera looks' **Face stickers** (`studio.stickers`: `effect_preset`, a preset of the
//! Effect Stack's `effects.json`) and the looks' **landmarks overlay** (`show_landmarks`), drawn
//! by the compositor on a source's picture (`crate::timed` runs them in the source's effect
//! order).
//!
//! A preset is a list of sprites: a picture from the module's `data/stickers`, placed at one of
//! the face's anchors (between the eyes, an eye, the nose, the mouth, the chin, the forehead,
//! the top of the head, a cheek), moved from it and sized in face units (the distance between the
//! eyes), turned with the head and by its own rotation, and animated (bob, pulse, spin, sway,
//! twinkle). Each is blended over the picture (normal, add or multiply) and fades in and out with
//! the face. A preset's face-shape distortion (its `shape`) is drawn by the camera looks' warp
//! ([`shape`], given to the engine's looks by `crate::looks`).
//!
//! The landmarks overlay is the tracked face's points as small green squares.
//!
//! The face comes from the camera looks' own analysis when it runs on the same camera
//! ([`share`], from `crate::looks`); otherwise a face tracker of the source's own analyses its
//! picture. Either way it's smoothed by the engine's face tracker and laid out in the face frame
//! by the engine's looks layout.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, Instant};

use lightair_engine::clock;
use lightair_engine::feeds::FacePush;
use lightair_engine::gpu::Gpu;
use lightair_engine::looks::Face;
use lightair_engine::looks::face::{FaceWeight, Tracker};
use lightair_engine::looks::layout::{self, Layout, Shape};
use lightair_host::bridge::{self, FaceTracker, RawFace};
use lightair_model::{Id, Source};
use parking_lot::Mutex;
use serde_json::Value;

pub(crate) const KIND: &str = "studio.stickers";
/// Not an effect kind: the step `crate::timed` adds for the landmarks overlay of a source with
/// no stickers.
pub(crate) const LANDMARKS: &str = "lightair.landmarks";

/// The preset a stickers effect without one shows (the catalog's default).
const DEFAULT_PRESET: &str = "Heart Eyes";

const PRESETS: &str = include_str!("../../../modules/lightair-effect-stack/data/effects.json");

macro_rules! art {
    ($name:literal) => {
        ($name, include_bytes!(concat!("../../../modules/lightair-effect-stack/data/stickers/", $name)).as_slice())
    };
}

/// The stickers' pictures (PNG), by file name.
const ART: [(&str, &[u8]); 18] = [
    art!("bunny_ears.png"),
    art!("cat_ears.png"),
    art!("crown.png"),
    art!("devil_horns.png"),
    art!("face_gems.png"),
    art!("flower_crown.png"),
    art!("freckles.png"),
    art!("halo.png"),
    art!("heart.png"),
    art!("heart_glasses.png"),
    art!("mustache.png"),
    art!("pixel_shades.png"),
    art!("puppy_ears.png"),
    art!("puppy_nose.png"),
    art!("puppy_tongue.png"),
    art!("sparkle.png"),
    art!("sunglasses.png"),
    art!("whiskers.png"),
];

/// The anchors' names, in the layout's anchor order.
const ANCHORS: [&str; layout::ANCHOR_COUNT] =
    ["eyes", "eye_l", "eye_r", "nose", "mouth", "chin", "forehead", "head_top", "cheek_l", "cheek_r"];
const BLENDS: [&str; 3] = ["normal", "add", "multiply"];
const ANIMS: [&str; 6] = ["none", "bob", "pulse", "spin", "sway", "twinkle"];

/// Shared faces older than this aren't used: the source's own tracker takes over.
const SHARED_FRESH: Duration = Duration::from_millis(500);
/// A source's own tracker not needed for this long is let go.
const IDLE: Duration = Duration::from_secs(3);
/// How long the faces of the camera looks' analysis are waited for (its models loading) before
/// the source's own tracker takes over.
const SHARED_WAIT: Duration = Duration::from_secs(2);
/// How often a picture goes to a source's own tracker (about 30 a second).
const ANALYSE_EVERY: Duration = Duration::from_millis(32);
/// How often a source's own analysis looks for new results while no picture comes.
const POLL: Duration = Duration::from_millis(8);
/// The most analysed faces kept for a reader.
const KEPT: usize = 8;

const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

// ---------------------------------------------------------------- the presets

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Blend {
    Normal,
    Add,
    Multiply,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Anim {
    None,
    Bob,
    Pulse,
    Spin,
    Sway,
    Twinkle,
}

/// One sticker of a preset.
#[derive(Debug, Clone)]
struct Sprite {
    /// Its picture's file name in `data/stickers`.
    image: String,
    anchor: usize,
    /// From the anchor, in face units.
    offset: [f32; 2],
    /// Its width in face units.
    width: f32,
    /// Degrees, on top of the head's roll.
    rotation: f32,
    blend: Blend,
    anim: Anim,
    anim_amt: f32,
    anim_speed: f32,
    phase: f32,
}

impl Sprite {
    fn parse(s: &Value) -> Sprite {
        let num = |k: &str| s[k].as_f64().unwrap_or(0.0) as f32;
        let index = |k: &str, names: &[&str]| names.iter().position(|n| s[k].as_str() == Some(n)).unwrap_or(0);
        Sprite {
            image: s["image"].as_str().unwrap_or("").to_string(),
            anchor: index("anchor", &ANCHORS),
            offset: [num("x"), num("y")],
            width: num("width"),
            rotation: num("rotation"),
            blend: [Blend::Normal, Blend::Add, Blend::Multiply][index("blend", &BLENDS)],
            anim: [Anim::None, Anim::Bob, Anim::Pulse, Anim::Spin, Anim::Sway, Anim::Twinkle][index("anim", &ANIMS)],
            anim_amt: num("anim_amt"),
            anim_speed: num("anim_speed"),
            phase: num("phase"),
        }
    }
}

/// One preset: its sprites and its face distortion.
struct Preset {
    sprites: Vec<Sprite>,
    /// Reshape keys to amounts (slider/100, may go past the sliders' range); none when it
    /// doesn't distort the face.
    shape: Option<serde_json::Map<String, Value>>,
}

/// Every preset, by name (the presets in `effects` at the top and in each category).
fn presets() -> &'static HashMap<String, Preset> {
    static ALL: OnceLock<HashMap<String, Preset>> = OnceLock::new();
    ALL.get_or_init(|| {
        let root: Value = serde_json::from_str(PRESETS).unwrap_or(Value::Null);
        let lists = std::iter::once(&root).chain(root["categories"].as_array().into_iter().flatten());
        let mut out = HashMap::new();
        for p in lists.filter_map(|l| l["effects"].as_array()).flatten() {
            let Some(name) = p["name"].as_str() else { continue };
            let sprites = p["sprites"].as_array().into_iter().flatten().take(8).map(Sprite::parse).collect();
            let shape = p["shape"].as_object().filter(|o| !o.is_empty()).cloned();
            out.entry(name.to_string()).or_insert(Preset { sprites, shape });
        }
        out
    })
}

/// The preset of a stickers effect's settings.
fn preset(settings: &Value) -> Option<&'static Preset> {
    let name = settings.get("effect_preset").and_then(Value::as_str).unwrap_or(DEFAULT_PRESET);
    presets().get(name)
}

/// The sprites of a stickers effect's preset (none for an unknown one, or one that only
/// distorts the face).
fn sprites(settings: &Value) -> &'static [Sprite] {
    preset(settings).map_or(&[], |p| p.sprites.as_slice())
}

/// The face distortion of a stickers effect's preset, when it has one.
pub(crate) fn shape(settings: &Value) -> Option<&'static serde_json::Map<String, Value>> {
    preset(settings).and_then(|p| p.shape.as_ref())
}

/// Whether `source`'s looks show the face's landmarks: the last switched-on look that says
/// (as the looks' settings are merged, `lightair_model::catalog::studio_settings`).
pub(crate) fn show_landmarks(source: &Source) -> bool {
    source
        .effects
        .iter()
        .filter(|e| e.enabled && e.kind.starts_with("studio."))
        .filter_map(|e| e.settings.get("show_landmarks"))
        .next_back()
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

// ---------------------------------------------------------------- the faces

/// One analysed picture's face, as the tracker gave it.
#[derive(Clone)]
struct Raw {
    time_ns: i64,
    found: bool,
    points: Vec<[f32; 2]>,
    size: (u32, u32),
}

impl Raw {
    fn of(f: &RawFace) -> Raw {
        Raw { time_ns: f.time_ns, found: f.found, points: f.points.clone(), size: f.size }
    }
}

/// The newest faces an analysis found, numbered.
#[derive(Default)]
struct Found {
    faces: VecDeque<(u64, Raw)>,
    at: Option<Instant>,
}

impl Found {
    fn add(&mut self, raw: Raw) {
        static NUMBER: AtomicU64 = AtomicU64::new(0);
        if self.faces.len() == KEPT {
            self.faces.pop_front();
        }
        self.faces.push_back((NUMBER.fetch_add(1, Ordering::Relaxed) + 1, raw));
        self.at = Some(Instant::now());
    }

    /// The faces numbered after `taken` (which becomes the newest's number).
    fn since(&self, taken: &mut u64) -> Vec<Raw> {
        let new: Vec<Raw> = self.faces.iter().filter(|(n, _)| *n > *taken).map(|(_, r)| r.clone()).collect();
        if let Some((n, _)) = self.faces.back() {
            *taken = (*taken).max(*n);
        }
        new
    }
}

/// The faces the camera looks' analysis finds, by source.
static SHARED: LazyLock<Mutex<HashMap<Id, Found>>> = LazyLock::new(Default::default);

/// The camera looks' analysis of `source` found `face` (or found none in its picture).
pub(crate) fn share(source: Id, face: &RawFace) {
    SHARED.lock().entry(source).or_default().add(Raw::of(face));
}

/// The camera looks' analysis of `source` has stopped.
pub(crate) fn unshare(source: Id) {
    SHARED.lock().remove(&source);
}

/// A picture for a source's own analysis.
struct Job(bridge::Buffer, i64);

// SAFETY: CoreVideo's buffers are reference counted thread-safely; a handed-on picture isn't
// written again while it's held.
unsafe impl Send for Job {}

/// A source's own face tracker, on a thread of its own.
struct Own {
    jobs: SyncSender<Job>,
    found: Arc<Mutex<Found>>,
    stop: Arc<AtomicBool>,
    sent: Option<Instant>,
    used: Instant,
}

impl Own {
    fn new() -> Option<Own> {
        let (jobs, rx) = mpsc::sync_channel(1);
        let found = Arc::new(Mutex::new(Found::default()));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (found, stop) = (found.clone(), stop.clone());
            std::thread::Builder::new()
                .name("lightair-stickers".into())
                .spawn(move || analyse(&rx, &found, &stop))
                .map_err(|e| tracing::error!(%e, "compositor: no thread for the stickers' face tracking"))
                .ok()?;
        }
        Some(Own { jobs, found, stop, sent: None, used: Instant::now() })
    }
}

impl Drop for Own {
    /// The thread is told to stop and not waited for (a tracker still loading its models takes a
    /// while to free).
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// A source's own analysis: each picture given scaled and handed to the tracker (landmarks only),
/// and the faces it finds kept.
fn analyse(jobs: &Receiver<Job>, found: &Mutex<Found>, stop: &AtomicBool) {
    let Some(tracker) = FaceTracker::new() else {
        tracing::warn!("compositor: no face tracker for the stickers");
        return;
    };
    tracker.segmentation(false, false, false, None);
    let mut scaler = crate::looks::Scaler::new();
    let (mut serial, mut id) = (0u64, 0u64);
    while !stop.load(Ordering::Acquire) {
        match jobs.recv_timeout(POLL) {
            Ok(Job(buffer, time_ns)) => {
                id += 1;
                scaler.submit(&tracker, &buffer, id, time_ns);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        if let Some(f) = tracker.raw_face(&mut serial) {
            found.lock().add(Raw::of(&f));
        }
    }
}

/// What a source's stickers see of its face this frame.
pub(crate) struct Seen {
    pub(crate) face: Face,
    /// Eases to 1 while a face is there and to 0 when not.
    pub(crate) weight: f32,
}

/// A source's face, followed across frames.
pub(crate) struct Faces {
    tracker: Tracker,
    weight: FaceWeight,
    /// The pushes given to the tracker (its serials).
    pushed: u64,
    /// The newest shared and own faces taken.
    shared_taken: u64,
    own_taken: u64,
    own: Option<Own>,
    /// When the shared faces were last fresh (or when these faces were first looked for).
    shared_at: Instant,
}

impl Default for Faces {
    fn default() -> Self {
        Faces {
            tracker: Tracker::new(),
            weight: FaceWeight::default(),
            pushed: 0,
            shared_taken: 0,
            own_taken: 0,
            own: None,
            shared_at: Instant::now(),
        }
    }
}

impl Faces {
    /// The face in `source`'s picture now (`buffer`: the picture, for its own tracker when the
    /// camera looks don't analyse it; `looks`: they're meant to, so their faces are waited for a
    /// moment first).
    pub(crate) fn see(&mut self, source: Id, buffer: Option<&bridge::Buffer>, looks: bool) -> Seen {
        let shared = SHARED
            .lock()
            .get(&source)
            .filter(|f| f.at.is_some_and(|t| t.elapsed() < SHARED_FRESH))
            .map(|f| f.since(&mut self.shared_taken));
        let new = match shared {
            Some(new) => {
                self.shared_at = Instant::now();
                if self.own.as_ref().is_some_and(|o| o.used.elapsed() > IDLE) {
                    self.own = None;
                }
                new
            }
            None if looks && self.own.is_none() && self.shared_at.elapsed() < SHARED_WAIT => Vec::new(),
            None => {
                if self.own.is_none() {
                    self.own = Own::new();
                }
                match self.own.as_mut() {
                    Some(own) => {
                        own.used = Instant::now();
                        if let Some(b) = buffer
                            && own.sent.is_none_or(|t| t.elapsed() >= ANALYSE_EVERY)
                        {
                            own.sent = Some(Instant::now());
                            // still analysing the last one: this picture is skipped
                            let _ = own.jobs.try_send(Job(b.clone(), clock::host_now_ns()));
                        }
                        own.found.lock().since(&mut self.own_taken)
                    }
                    None => Vec::new(),
                }
            }
        };
        for raw in new {
            self.pushed += 1;
            let push = FacePush {
                pts_ns: raw.time_ns,
                present: raw.found,
                score: if raw.found { 1.0 } else { 0.0 },
                points: raw.points,
                size: raw.size,
                serial: self.pushed,
            };
            self.tracker.push(&push, None);
        }
        let now = clock::host_now_ns();
        let face = self.tracker.get_at(now, now);
        let weight = self.weight.update(face.present, now);
        Seen { face, weight }
    }
}

// ---------------------------------------------------------------- drawing

const SHADER: &str = r#"
struct Out {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
    @location(1) opacity: f32,
};

@group(0) @binding(0) var art: texture_2d<f32>;
@group(0) @binding(1) var linear: sampler;

@vertex
fn vs(@location(0) pos: vec2<f32>, @location(1) uv: vec2<f32>, @location(2) opacity: f32) -> Out {
    var o: Out;
    o.pos = vec4<f32>(pos, 0.0, 1.0);
    o.uv = uv;
    o.opacity = opacity;
    return o;
}

// a picture (premultiplied) at the vertex's opacity
@fragment
fn picture(v: Out) -> @location(0) vec4<f32> {
    return textureSample(art, linear, v.uv) * v.opacity;
}

// a landmark's square
@fragment
fn solid(v: Out) -> @location(0) vec4<f32> {
    return vec4<f32>(0.2, 1.0, 0.4, 1.0) * v.opacity;
}
"#;

/// One vertex: its place (clip space), its picture's uv and opacity.
type Vertex = [f32; 5];

/// What one stickers draw shows.
#[derive(Clone, Copy)]
pub(crate) struct Look<'a> {
    pub(crate) seen: &'a Seen,
    /// The stickers effect's settings (its preset); none: no stickers.
    pub(crate) settings: Option<&'a Value>,
    /// Seconds into the stickers' animations.
    pub(crate) t: f32,
    /// The face's points drawn over the stickers.
    pub(crate) landmarks: bool,
}

/// A sticker's picture on the GPU (premultiplied, with mipmaps) and its size.
struct Art {
    group: wgpu::BindGroup,
    size: (u32, u32),
}

/// The stickers' pipelines and pictures, made once.
pub(crate) struct Stickers {
    copy: wgpu::RenderPipeline,
    normal: wgpu::RenderPipeline,
    add: wgpu::RenderPipeline,
    multiply: wgpu::RenderPipeline,
    dots: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    art: HashMap<String, Option<Art>>,
}

impl Stickers {
    pub(crate) fn new(device: &wgpu::Device) -> Stickers {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("stickers"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("stickers"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: true },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("stickers"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let attributes = [
            wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 0, shader_location: 0 },
            wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 8, shader_location: 1 },
            wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32, offset: 16, shader_location: 2 },
        ];
        let buffers = [Some(wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as u64,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &attributes,
        })];
        let pipeline = |entry: &str, blend: Option<wgpu::BlendState>| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("stickers"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs"),
                    compilation_options: Default::default(),
                    buffers: &buffers,
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(entry),
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
        let component = |src_factor, dst_factor| wgpu::BlendComponent {
            src_factor,
            dst_factor,
            operation: wgpu::BlendOperation::Add,
        };
        use wgpu::BlendFactor::{Dst, One, OneMinusSrcAlpha, Zero};
        let over = component(One, OneMinusSrcAlpha);
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("stickers"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            ..Default::default()
        });
        Stickers {
            copy: pipeline("picture", None),
            normal: pipeline("picture", Some(wgpu::BlendState { color: over, alpha: over })),
            // the sticker's light added; the picture's alpha as over it
            add: pipeline("picture", Some(wgpu::BlendState { color: component(One, One), alpha: over })),
            // the picture darkened by the sticker's colour where it covers it; its alpha kept
            multiply: pipeline(
                "picture",
                Some(wgpu::BlendState { color: component(Dst, OneMinusSrcAlpha), alpha: component(Zero, One) }),
            ),
            dots: pipeline("solid", Some(wgpu::BlendState { color: over, alpha: over })),
            layout,
            sampler,
            art: HashMap::new(),
        }
    }

    /// A sticker's picture, loaded the first time it's asked for.
    fn art(&mut self, gpu: &Gpu, name: &str) -> Option<&Art> {
        if !self.art.contains_key(name) {
            let art = load(gpu, name).map(|(t, size)| Art { group: self.group(gpu.device(), &t), size });
            if art.is_none() {
                tracing::warn!(name, "compositor: a face sticker's picture is missing or unreadable");
            }
            self.art.insert(name.to_string(), art);
        }
        self.art.get(name)?.as_ref()
    }

    fn group(&self, device: &wgpu::Device, texture: &wgpu::Texture) -> wgpu::BindGroup {
        let view = texture.create_view(&Default::default());
        device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("stickers"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.sampler) },
            ],
        })
    }

    /// Draws `from` into `out` (same size) with `look` over it. False when there's nothing to
    /// draw (`out` is then left as it was).
    pub(crate) fn draw(&mut self, gpu: &Gpu, from: &wgpu::Texture, out: &wgpu::Texture, look: &Look) -> bool {
        let Look { seen, settings, t, landmarks } = *look;
        let (w, h) = (out.width() as f32, out.height() as f32);
        let clip = |p: [f32; 2]| [p[0] / w * 2.0 - 1.0, 1.0 - p[1] / h * 2.0];
        let mut vertices: Vec<Vertex> = Vec::new();
        let quad = |vertices: &mut Vec<Vertex>, corners: [[f32; 2]; 4], opacity: f32| {
            let uv = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
            for i in [0, 1, 2, 0, 2, 3] {
                let p = clip(corners[i]);
                vertices.push([p[0], p[1], uv[i][0], uv[i][1], opacity]);
            }
        };
        // the picture as it is
        quad(&mut vertices, [[0.0, 0.0], [w, 0.0], [w, h], [0.0, h]], 1.0);

        // the stickers, each with the draw it's in (its pipeline and picture)
        let mut draws: Vec<(Blend, String)> = Vec::new();
        let sprites = settings.map_or(&[][..], sprites);
        let l: Layout = layout::build(Some(&seen.face), &Shape::default(), w, h, seen.weight, 0);
        if l.has_face && seen.weight > 0.01 {
            let roll = l.axis_u[1].atan2(l.axis_u[0]);
            for s in sprites {
                if s.width <= 0.0 {
                    continue;
                }
                let Some(art) = self.art(gpu, &s.image) else { continue };
                let (tw, th) = (art.size.0.max(1) as f32, art.size.1.max(1) as f32);
                let ph = t * s.anim_speed * std::f32::consts::TAU + s.phase * std::f32::consts::TAU;
                let ox = s.offset[0];
                let (mut oy, mut scale, mut rot, mut alpha) = (s.offset[1], 1.0, s.rotation, seen.weight);
                match s.anim {
                    Anim::Bob => oy += ph.sin() * s.anim_amt,
                    Anim::Pulse => scale += ph.sin() * s.anim_amt,
                    Anim::Spin => rot += t * s.anim_speed * 360.0,
                    Anim::Sway => rot += ph.sin() * s.anim_amt * 25.0,
                    Anim::Twinkle => {
                        alpha *= 0.55 + 0.45 * ph.sin();
                        scale += 0.08 * ph.sin();
                    }
                    Anim::None => {}
                }
                let a = l.anchor[s.anchor.min(layout::ANCHOR_COUNT - 1)];
                let [cx, cy] = layout::map(&l, a[0] + ox, a[1] + oy);
                let wpx = s.width * l.iod_px * scale;
                let hpx = wpx * th / tw;
                // the sticker's x along the eyes, turned by its own rotation
                let (sn, cs) = (roll + rot.to_radians()).sin_cos();
                let corner = |qx: f32, qy: f32| {
                    let (x, y) = (qx * wpx, qy * hpx);
                    [cx + x * cs - y * sn, cy + x * sn + y * cs]
                };
                quad(
                    &mut vertices,
                    [corner(-0.5, -0.5), corner(0.5, -0.5), corner(0.5, 0.5), corner(-0.5, 0.5)],
                    alpha.clamp(0.0, 1.0),
                );
                draws.push((s.blend, s.image.clone()));
            }
        }

        // the landmarks: squares of `dot` pixels
        let mut dots = 0;
        if landmarks && seen.face.valid {
            let dot = (h / 300.0).max(2.0);
            for p in &seen.face.pts {
                let (x, y) = (p[0] * w - dot * 0.5, p[1] * h - dot * 0.5);
                quad(&mut vertices, [[x, y], [x + dot, y], [x + dot, y + dot], [x, y + dot]], 1.0);
                dots += 1;
            }
        }
        if draws.is_empty() && dots == 0 {
            return false;
        }

        let device = gpu.device();
        let bytes: Vec<u8> = vertices.iter().flatten().flat_map(|v| v.to_le_bytes()).collect();
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("stickers"),
            size: bytes.len() as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        gpu.queue().write_buffer(&buffer, 0, &bytes);
        let picture = self.group(device, from);
        let view = out.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("stickers"),
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
            rp.set_vertex_buffer(0, buffer.slice(..));
            rp.set_pipeline(&self.copy);
            rp.set_bind_group(0, &picture, &[]);
            rp.draw(0..6, 0..1);
            let mut at = 6;
            for (blend, image) in &draws {
                let range = at..at + 6;
                at += 6;
                let Some(Some(art)) = self.art.get(image) else { continue };
                rp.set_pipeline(match blend {
                    Blend::Normal => &self.normal,
                    Blend::Add => &self.add,
                    Blend::Multiply => &self.multiply,
                });
                rp.set_bind_group(0, &art.group, &[]);
                rp.draw(range, 0..1);
            }
            if dots > 0 {
                rp.set_pipeline(&self.dots);
                rp.set_bind_group(0, &picture, &[]);
                rp.draw(at..at + dots * 6, 0..1);
            }
        }
        gpu.queue().submit([encoder.finish()]);
        true
    }
}

/// A sticker's picture (by file name) as a premultiplied, mipmapped texture, and its size.
fn load(gpu: &Gpu, name: &str) -> Option<(wgpu::Texture, (u32, u32))> {
    let (_, png) = ART.iter().find(|(n, _)| *n == name)?;
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let mut rgba = img.into_raw();
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        for c in &mut px[..3] {
            *c = ((*c as u32 * a + 127) / 255) as u8;
        }
    }
    let levels = lightair_engine::looks::mesh::mip_chain(rgba, w, h);
    let t = gpu.device().create_texture(&wgpu::TextureDescriptor {
        label: Some("sticker"),
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: levels.len() as u32,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    for (level, (bytes, lw, lh)) in levels.iter().enumerate() {
        gpu.queue().write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &t,
                mip_level: level as u32,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            bytes,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(lw * 4), rows_per_image: Some(*lh) },
            wgpu::Extent3d { width: *lw, height: *lh, depth_or_array_layers: 1 },
        );
    }
    Some((t, (w, h)))
}
