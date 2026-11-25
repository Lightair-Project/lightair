//! Image slideshows (`slideshow` and `slideshow_v2`, as workspaces and OBS imports hold them),
//! drawn by the compositor itself: the slide now, and the switch between two slides.
//!
//! Settings: `files` (a list of `{ value, hidden }`; a folder stands for the images in it, by
//! name), `slide_time` and `transition_speed` (milliseconds), `transition` (`fade`, `cut`,
//! `slide`, `swipe`), `loop`, `randomize`, `hide` (gone after the last slide), `slide_mode`
//! (`mode_manual`: the first slide stays, as nothing here changes slides by hand),
//! `playback_behavior` (`always_play`, `stop_restart`, `pause_unpause`) and `use_custom_size`
//! (`WxH`, `W:H`, or anything else for the largest image's size).
//!
//! Each slide is fitted into the slideshow's size, centred, as libobs's slideshow fits them.
//! A fade crossfades the two slides; a slide moves both to the left; a swipe moves the old one
//! out to the left over the new one. The list and the images' sizes are read on a thread, and
//! only the slides before, at and after the one now are kept, each decoded on a thread of its
//! own (a GIF shows its first frame). A slide not decoded yet leaves the last picture drawn.
//!
//! The time a slideshow has run is shared by every compositor (one per canvas), so the same
//! slideshow shows the same slide on each.

use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use lightair_engine::gpu::Gpu;
use lightair_model::{Id, Source};
use lightair_render::media::Picture;
use lightair_render::scene::Shot;
use parking_lot::Mutex;
use serde_json::Value;

/// The source kinds drawn here.
pub(crate) const KINDS: &[&str] = &["slideshow", "slideshow_v2"];

/// A slideshow not drawn for this long was hidden (it restarts or pauses, as its settings say).
const AWAY: Duration = Duration::from_secs(1);
/// The largest side a slide is kept at.
const MOST_SIDE: u32 = 4096;
/// Image files a folder's slides are taken from.
const EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp"];

/// Whether `source` is a slideshow with images listed.
pub(crate) fn drawn(source: &Source) -> bool {
    KINDS.contains(&source.kind.as_str()) && !files(&source.settings).is_empty()
}

// ---------------------------------------------------------------- settings

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transition {
    Cut,
    Fade,
    Slide,
    Swipe,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Behavior {
    Always,
    Restart,
    Pause,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Size {
    Largest,
    Exact(u32, u32),
    Aspect(u32, u32),
}

/// A slideshow's settings, with libobs's defaults.
#[derive(Debug, Clone, PartialEq)]
struct Params {
    files: Vec<PathBuf>,
    slide_ms: u64,
    speed_ms: u64,
    transition: Transition,
    looped: bool,
    random: bool,
    hide: bool,
    manual: bool,
    behavior: Behavior,
    size: Size,
}

/// The listed files and folders, the hidden ones left out.
fn files(s: &Value) -> Vec<PathBuf> {
    let Some(list) = s["files"].as_array() else { return Vec::new() };
    list.iter()
        .filter_map(|f| match f {
            Value::String(p) => Some(p.as_str()),
            Value::Object(_) if !f["hidden"].as_bool().unwrap_or(false) => f["value"].as_str(),
            _ => None,
        })
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Two numbers split by `sep` ("1920x1080", "16:9").
fn pair(s: &str, sep: char) -> Option<(u32, u32)> {
    let (a, b) = s.split_once(sep)?;
    let (a, b) = (a.trim().parse::<u32>().ok()?, b.trim().parse::<u32>().ok()?);
    (a > 0 && b > 0).then_some((a, b))
}

impl Params {
    fn of(s: &Value) -> Params {
        let int = |k: &str, d: u64| s[k].as_u64().unwrap_or(d);
        let flag = |k: &str, d: bool| s[k].as_bool().unwrap_or(d);
        let transition = match s["transition"].as_str().unwrap_or("fade") {
            "cut" => Transition::Cut,
            "slide" => Transition::Slide,
            "swipe" => Transition::Swipe,
            _ => Transition::Fade,
        };
        let speed_ms = if transition == Transition::Cut { 0 } else { int("transition_speed", 700) };
        let size = s["use_custom_size"].as_str().unwrap_or("");
        let size = match (pair(size, 'x'), pair(size, ':')) {
            (Some((w, h)), _) => Size::Exact(w.min(MOST_SIDE), h.min(MOST_SIDE)),
            (None, Some((w, h))) => Size::Aspect(w, h),
            _ => Size::Largest,
        };
        Params {
            files: files(s),
            // a slide lasts at least as long as the switch to it
            slide_ms: int("slide_time", 8000).max(speed_ms + 50).max(50),
            speed_ms,
            transition,
            looped: flag("loop", true),
            random: flag("randomize", false),
            hide: flag("hide", false),
            manual: s["slide_mode"].as_str() == Some("mode_manual"),
            behavior: match s["playback_behavior"].as_str() {
                Some("stop_restart") => Behavior::Restart,
                Some("pause_unpause") => Behavior::Pause,
                _ => Behavior::Always,
            },
            size,
        }
    }

    /// What decides the show's order and timing (a change starts it again).
    fn key(&self) -> u64 {
        let mut h = DefaultHasher::new();
        format!("{self:?}").hash(&mut h);
        h.finish()
    }
}

// ---------------------------------------------------------------- the files and the size

/// The slides' files and the slideshow's size.
struct Plan {
    files: Vec<PathBuf>,
    size: (u32, u32),
}

/// The listed files, folders' images in them by name, and the size: the largest image's (or
/// the custom one).
fn plan(p: &Params) -> Option<Plan> {
    let mut files = Vec::new();
    for f in &p.files {
        if f.is_dir() {
            let Ok(dir) = std::fs::read_dir(f) else { continue };
            let mut found: Vec<PathBuf> = dir
                .filter_map(|e| Some(e.ok()?.path()))
                .filter(|p| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
                })
                .collect();
            found.sort();
            files.extend(found);
        } else {
            files.push(f.clone());
        }
    }
    let largest = files
        .iter()
        .filter_map(|f| image::image_dimensions(f).ok())
        .fold((0, 0), |(w, h), (a, b)| (w.max(a), h.max(b)));
    let size = match p.size {
        Size::Exact(w, h) => (w, h),
        Size::Largest => largest,
        Size::Aspect(aw, ah) => {
            let (w, h) = (largest.0 as f64, largest.1 as f64);
            let (want, now) = (aw as f64 / ah as f64, w / h.max(1.0));
            if want > now { ((h * want) as u32, largest.1) } else { (largest.0, (w / want) as u32) }
        }
    };
    let size = (size.0.min(MOST_SIDE), size.1.min(MOST_SIDE));
    (size.0 > 0 && size.1 > 0 && !files.is_empty()).then_some(Plan { files, size })
}

/// A slide's picture, fitted within `bound` when it's larger, colours multiplied by alpha.
fn decode(path: &Path, bound: (u32, u32)) -> Option<Decoded> {
    let img =
        image::open(path).map_err(|e| tracing::warn!(path = %path.display(), %e, "a slide that can't be read")).ok()?;
    let img = if img.width() > bound.0 || img.height() > bound.1 { img.thumbnail(bound.0, bound.1) } else { img };
    let mut rgba = img.to_rgba8();
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = px[3] as u32;
        if a < 255 {
            for c in &mut px[..3] {
                *c = ((*c as u32 * a + 127) / 255) as u8;
            }
        }
    }
    let size = rgba.dimensions();
    (size.0 > 0 && size.1 > 0).then(|| (rgba.into_raw(), size))
}

// ---------------------------------------------------------------- timing

/// How long each slideshow has run (shared by every compositor).
struct Run {
    key: u64,
    start: Instant,
    seen: Instant,
}

static RUNS: LazyLock<Mutex<HashMap<Id, Run>>> = LazyLock::new(Default::default);

/// How long slideshow `id` (settings `key`) has run, drawn now: started again or paused while it
/// wasn't drawn, as `behavior` says; from the start when its settings change.
fn elapsed(id: Id, key: u64, behavior: Behavior) -> Duration {
    let now = Instant::now();
    let mut runs = RUNS.lock();
    let run = runs.entry(id).or_insert(Run { key, start: now, seen: now });
    if run.key != key {
        *run = Run { key, start: now, seen: now };
    }
    let away = now - run.seen;
    if away > AWAY {
        match behavior {
            Behavior::Restart => run.start = now,
            Behavior::Pause => run.start += away,
            Behavior::Always => {}
        }
    }
    run.seen = now;
    now - run.start
}

/// What's shown: slide `a` going to slide `b` (`None`: nothing) at `p` (0: `a` alone).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Moment {
    a: Option<usize>,
    b: Option<usize>,
    p: f32,
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

/// Round `round` of a random show: every slide once, in an order of its own.
fn shuffled(seed: u64, round: u64, n: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..n).collect();
    let mut r = mix(seed ^ mix(round));
    for i in (1..n).rev() {
        r = mix(r);
        order.swap(i, (r % (i as u64 + 1)) as usize);
    }
    order
}

/// The slide at step `k` of `n` (random: never the same twice in a row).
fn slide_at(k: u64, n: usize, random: bool, seed: u64) -> usize {
    if !random || n < 2 {
        return (k % n as u64) as usize;
    }
    let (round, at) = (k / n as u64, (k % n as u64) as usize);
    let mut order = shuffled(seed, round, n);
    if round > 0 && shuffled(seed, round - 1, n).last() == order.first() {
        order.swap(0, 1);
    }
    order[at]
}

impl Params {
    /// What's shown `t` into the show of `n` slides, and the step it's at.
    fn moment(&self, t: Duration, n: usize, seed: u64) -> (Moment, u64) {
        let at = |k: u64| slide_at(k, n, self.random, seed);
        if self.manual {
            return (Moment { a: Some(at(0)), b: None, p: 0.0 }, 0);
        }
        let ms = t.as_millis() as u64;
        let (k, within) = (ms / self.slide_ms, ms % self.slide_ms);
        let switching = |k: u64| within < self.speed_ms && k > 0;
        let p = within as f32 / self.speed_ms.max(1) as f32;
        let last = n as u64 - 1;
        if !self.random && !self.looped && k > last {
            // done: the last slide stays, or goes
            if !self.hide {
                return (Moment { a: Some(at(last)), b: None, p: 0.0 }, last);
            }
            if k == last + 1 && switching(k) {
                return (Moment { a: Some(at(last)), b: None, p }, last);
            }
            return (Moment { a: None, b: None, p: 0.0 }, last);
        }
        if switching(k) {
            return (Moment { a: Some(at(k - 1)), b: Some(at(k)), p }, k);
        }
        (Moment { a: Some(at(k)), b: None, p: 0.0 }, k)
    }
}

// ---------------------------------------------------------------- drawing

/// Draws a slide, or two in a switch, into the slideshow's picture.
const SHADER: &str = r#"
struct U {
    rect_a: vec4<f32>,
    rect_b: vec4<f32>,
    shift: vec4<f32>,
    // x: how much of b (a fade), y: 0 fade, 1 b over a, 2 a over b, z/w: a/b drawn
    how: vec4<f32>,
}

@group(0) @binding(0) var<uniform> u: U;
@group(0) @binding(1) var ta: texture_2d<f32>;
@group(0) @binding(2) var tb: texture_2d<f32>;
@group(0) @binding(3) var smp: sampler;

struct V {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs(@builtin(vertex_index) i: u32) -> V {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var o: V;
    o.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    o.uv = uv;
    return o;
}

fn inside(t: texture_2d<f32>, r: vec4<f32>, p: vec2<f32>) -> vec4<f32> {
    let q = (p - r.xy) / max(r.zw - r.xy, vec2<f32>(1e-6));
    let c = textureSampleLevel(t, smp, clamp(q, vec2<f32>(0.0), vec2<f32>(1.0)), 0.0);
    let within = all(q >= vec2<f32>(0.0)) && all(q <= vec2<f32>(1.0));
    return select(vec4<f32>(0.0), c, within);
}

@fragment
fn fs(v: V) -> @location(0) vec4<f32> {
    let a = inside(ta, u.rect_a, v.uv - u.shift.xy) * u.how.z;
    let b = inside(tb, u.rect_b, v.uv - u.shift.zw) * u.how.w;
    if (u.how.y < 0.5) {
        return a * (1.0 - u.how.x) + b * u.how.x;
    }
    if (u.how.y < 1.5) {
        return b + a * (1.0 - b.a);
    }
    return a + b * (1.0 - a.a);
}
"#;

/// The pass that draws slides (made with the first slideshow).
struct Pass {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Bound where a slide isn't drawn.
    blank: wgpu::Texture,
}

impl Pass {
    fn new(gpu: &Gpu) -> Pass {
        let device = gpu.device();
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("slideshow"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        let entry = |binding, ty| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty,
            count: None,
        };
        let texture = wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        };
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("slideshow"),
            entries: &[
                entry(
                    0,
                    wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                ),
                entry(1, texture),
                entry(2, texture),
                entry(3, wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering)),
            ],
        });
        let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("slideshow"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("slideshow"),
            layout: Some(&pl),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: wgpu::TextureFormat::Rgba8Unorm,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("slideshow"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let blank = gpu.texture(&[0; 4], (1, 1));
        Pass { pipeline, layout, sampler, blank }
    }
}

/// A slide's pixels (colours multiplied by alpha) and size.
type Decoded = (Vec<u8>, (u32, u32));

/// A slide: being decoded, ready, or unreadable.
enum Slide {
    Loading(Receiver<Option<Decoded>>),
    Ready(wgpu::Texture),
    Broken,
}

/// One slideshow as one compositor draws it.
struct Show {
    params: Params,
    key: u64,
    seed: u64,
    plan: Option<Plan>,
    /// The list and size, while they're read.
    planning: Option<Receiver<Option<Plan>>>,
    /// The slides kept, by their place in the plan's files.
    slides: HashMap<usize, Slide>,
    /// The picture as last drawn, and what it was drawn from.
    out: Option<(wgpu::Texture, Drawn)>,
    uniform: Option<wgpu::Buffer>,
}

/// What a picture was drawn from (drawn again when it changes).
#[derive(Debug, Clone, PartialEq)]
struct Drawn {
    a: Option<usize>,
    b: Option<usize>,
    values: [f32; 16],
}

impl Show {
    fn new(source: &Source) -> Show {
        let params = Params::of(&source.settings);
        let key = params.key();
        let mut h = DefaultHasher::new();
        source.id.hash(&mut h);
        key.hash(&mut h);
        let (tx, rx) = channel();
        let p = params.clone();
        let started = std::thread::Builder::new()
            .name("lightair-slideshow".into())
            .spawn(move || {
                let _ = tx.send(plan(&p));
            })
            .is_ok();
        Show {
            params,
            key,
            seed: h.finish(),
            plan: None,
            planning: started.then_some(rx),
            slides: HashMap::new(),
            out: None,
            uniform: None,
        }
    }

    /// Slide `i`'s texture, decoding it when it isn't kept (`None` until it's ready).
    fn slide(&mut self, gpu: &Gpu, i: usize) -> Option<wgpu::Texture> {
        let plan = self.plan.as_ref()?;
        let slot = self.slides.entry(i).or_insert_with(|| {
            let (tx, rx) = channel();
            let (path, bound) = (plan.files[i].clone(), plan.size);
            let started = std::thread::Builder::new()
                .name("lightair-slide".into())
                .spawn(move || {
                    let _ = tx.send(decode(&path, bound));
                })
                .is_ok();
            if started { Slide::Loading(rx) } else { Slide::Broken }
        });
        if let Slide::Loading(rx) = slot {
            *slot = match rx.try_recv() {
                Ok(Some((rgba, size))) => Slide::Ready(gpu.texture(&rgba, size)),
                Ok(None) | Err(TryRecvError::Disconnected) => Slide::Broken,
                Err(TryRecvError::Empty) => return None,
            };
        }
        match slot {
            Slide::Ready(t) => Some(t.clone()),
            _ => None,
        }
    }

    /// The picture now; `None` until there's one.
    fn picture(&mut self, gpu: &Gpu, pass: &Pass, id: Id) -> Option<Shot> {
        if let Some(rx) = &self.planning {
            match rx.try_recv() {
                Ok(plan) => {
                    self.plan = plan;
                    self.planning = None;
                }
                Err(TryRecvError::Empty) => return None,
                Err(TryRecvError::Disconnected) => self.planning = None,
            }
        }
        let (n, size) = {
            let plan = self.plan.as_ref()?;
            (plan.files.len(), plan.size)
        };
        let t = elapsed(id, self.key, self.params.behavior);
        let (moment, step) = self.params.moment(t, n, self.seed);
        // the slides before, at and after this step are kept; the next one is read ahead
        let near: Vec<usize> = [step.saturating_sub(1), step, step + 1]
            .iter()
            .map(|&k| slide_at(k, n, self.params.random, self.seed))
            .collect();
        self.slides.retain(|i, _| near.contains(i));
        if !self.params.manual {
            let _ = self.slide(gpu, near[2]);
        }
        let a = moment.a.and_then(|i| Some((i, self.slide(gpu, i)?)));
        let b = moment.b.and_then(|i| Some((i, self.slide(gpu, i)?)));
        let wanted = moment.a.is_some() || moment.b.is_some();
        if wanted && a.is_none() && b.is_none() {
            // nothing ready yet: the last picture stays
            return self.out.as_ref().map(|(t, _)| shot(t));
        }
        // a switch to a slide not ready yet waits on the slide before it
        let p = if b.is_some() || moment.b.is_none() { moment.p } else { 0.0 };
        let (a, b, p) = if a.is_none() { (b, None, 0.0) } else { (a, b, p) };
        let values = self.values(size, a.as_ref().map(|s| &s.1), b.as_ref().map(|s| &s.1), p);
        let drawn = Drawn { a: a.as_ref().map(|s| s.0), b: b.as_ref().map(|s| s.0), values };
        if let Some((t, last)) = &self.out
            && (t.width(), t.height()) == size
            && *last == drawn
        {
            return Some(shot(t));
        }
        let target = match self.out.take() {
            Some((t, _)) if (t.width(), t.height()) == size => t,
            _ => gpu.target(size, wgpu::TextureFormat::Rgba8Unorm),
        };
        self.draw(gpu, pass, &target, a.map(|s| s.1), b.map(|s| s.1), &values);
        let out = shot(&target);
        self.out = Some((target, drawn));
        Some(out)
    }

    /// The pass's values: where each slide sits, how they move and mix.
    fn values(&self, size: (u32, u32), a: Option<&wgpu::Texture>, b: Option<&wgpu::Texture>, p: f32) -> [f32; 16] {
        let fit = |t: Option<&wgpu::Texture>| -> [f32; 4] {
            let Some(t) = t else { return [0.0; 4] };
            let (w, h) = (t.width() as f32, t.height() as f32);
            let s = (size.0 as f32 / w).min(size.1 as f32 / h);
            let (rw, rh) = (w * s / size.0 as f32, h * s / size.1 as f32);
            let (x, y) = ((1.0 - rw) * 0.5, (1.0 - rh) * 0.5);
            [x, y, x + rw, y + rh]
        };
        // slides and swipes ease in and out
        let e = if p < 0.5 { 4.0 * p * p * p } else { 1.0 - (-2.0 * p + 2.0).powi(3) * 0.5 };
        let (shift, mode, amount) = match self.params.transition {
            Transition::Cut | Transition::Fade => ([0.0; 4], 0.0, p),
            Transition::Slide => ([-e, 0.0, 1.0 - e, 0.0], 1.0, 0.0),
            Transition::Swipe => ([-e, 0.0, 0.0, 0.0], 2.0, 0.0),
        };
        let (ra, rb) = (fit(a), fit(b));
        let drawn = |t: Option<&wgpu::Texture>| if t.is_some() { 1.0 } else { 0.0 };
        [
            ra[0],
            ra[1],
            ra[2],
            ra[3],
            rb[0],
            rb[1],
            rb[2],
            rb[3],
            shift[0],
            shift[1],
            shift[2],
            shift[3],
            amount,
            mode,
            drawn(a),
            drawn(b),
        ]
    }

    fn draw(
        &mut self,
        gpu: &Gpu,
        pass: &Pass,
        target: &wgpu::Texture,
        a: Option<wgpu::Texture>,
        b: Option<wgpu::Texture>,
        values: &[f32; 16],
    ) {
        let device = gpu.device();
        let uniform = self.uniform.get_or_insert_with(|| {
            device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("slideshow"),
                size: 64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            })
        });
        let bytes: Vec<u8> = values.iter().flat_map(|v| v.to_le_bytes()).collect();
        gpu.queue().write_buffer(uniform, 0, &bytes);
        let va = a.as_ref().unwrap_or(&pass.blank).create_view(&Default::default());
        let vb = b.as_ref().unwrap_or(&pass.blank).create_view(&Default::default());
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("slideshow"),
            layout: &pass.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&va) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::TextureView(&vb) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&pass.sampler) },
            ],
        });
        let view = target.create_view(&Default::default());
        let mut encoder = device.create_command_encoder(&Default::default());
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("slideshow"),
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
            rp.set_pipeline(&pass.pipeline);
            rp.set_bind_group(0, &group, &[]);
            rp.draw(0..3, 0..1);
        }
        gpu.queue().submit([encoder.finish()]);
    }
}

fn shot(t: &wgpu::Texture) -> Shot {
    Shot { picture: Picture::Texture(t.clone()), size: (t.width(), t.height()), premultiplied: true }
}

/// The slideshows one compositor draws.
#[derive(Default)]
pub(crate) struct Slideshows {
    pass: Option<Pass>,
    shows: HashMap<Id, Show>,
}

impl Slideshows {
    /// Slideshow `source`'s picture now; `None` until its first slide is ready.
    pub(crate) fn picture(&mut self, gpu: &Gpu, source: &Source) -> Option<Shot> {
        let pass = self.pass.get_or_insert_with(|| Pass::new(gpu));
        let params = Params::of(&source.settings);
        if self.shows.get(&source.id).is_none_or(|s| s.params != params) {
            self.shows.insert(source.id, Show::new(source));
        }
        self.shows.get_mut(&source.id)?.picture(gpu, pass, source.id)
    }

    /// Lets go of the slideshows `keep` says are gone.
    pub(crate) fn keep(&mut self, keep: impl Fn(Id) -> bool) {
        self.shows.retain(|id, _| keep(*id));
    }
}
