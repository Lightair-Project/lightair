//! Plugin hosting (docs/plans/audio-engine.md, Plugins): Audio Units (v2 and v3) through the
//! AudioToolbox C API. AUv3 units load out of process when the system chooses to, so a crashing
//! one is reported rather than fatal.
//!
//! A plugin insert in the project ("audio.plugin": format, id, name, vendor, state) gets its
//! instance here, keyed by the effect's id. Loading is asynchronous: until it's ready the insert
//! passes audio through, and [`changed`] tells the reconciler to play the mixer again.

use std::cell::UnsafeCell;
use std::collections::{HashMap, HashSet};
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use base64::Engine as _;
use lightair_audio::dsp::Plugin;
use lightair_core::{Effect, Id};
use parking_lot::Mutex;

/// An installed plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginInfo {
    /// "au" (VST3 later: "vst3").
    pub format: &'static str,
    /// The format's identifier ("au:<type>:<subtype>:<manufacturer>", hex).
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub version: String,
    /// An AUv3 app extension (out of process).
    pub v3: bool,
}

// ------------------------------------------------------------------ AudioToolbox

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
struct Desc {
    ty: u32,
    sub: u32,
    manu: u32,
    flags: u32,
    mask: u32,
}

#[repr(C)]
#[derive(Default)]
struct Asbd {
    rate: f64,
    format: u32,
    flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels: u32,
    bits: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Default)]
struct Smpte {
    subframes: i16,
    divisor: i16,
    counter: u32,
    ty: u32,
    flags: u32,
    hours: i16,
    minutes: i16,
    seconds: i16,
    frames: i16,
}

#[repr(C)]
#[derive(Default)]
struct TimeStamp {
    sample_time: f64,
    host_time: u64,
    rate_scalar: f64,
    word_clock: u64,
    smpte: Smpte,
    flags: u32,
    reserved: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Buffer {
    channels: u32,
    size: u32,
    data: *mut c_void,
}

/// An AudioBufferList with room for two buffers.
#[repr(C)]
struct BufferList {
    n: u32,
    bufs: [Buffer; 2],
}

type RenderProc = unsafe extern "C" fn(*mut c_void, *mut u32, *const TimeStamp, u32, u32, *mut BufferList) -> i32;

#[repr(C)]
struct RenderCallback {
    proc_: RenderProc,
    refcon: *mut c_void,
}

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioComponentFindNext(c: *mut c_void, desc: *const Desc) -> *mut c_void;
    fn AudioComponentCopyName(c: *mut c_void, name: *mut *const c_void) -> i32;
    fn AudioComponentGetDescription(c: *mut c_void, desc: *mut Desc) -> i32;
    fn AudioComponentGetVersion(c: *mut c_void, version: *mut u32) -> i32;
    fn AudioComponentInstantiate(c: *mut c_void, options: u32, handler: &block2::DynBlock<dyn Fn(*mut c_void, i32)>);
    fn AudioComponentInstanceDispose(unit: *mut c_void) -> i32;
    fn AudioUnitSetProperty(unit: *mut c_void, id: u32, scope: u32, el: u32, data: *const c_void, size: u32) -> i32;
    fn AudioUnitGetProperty(unit: *mut c_void, id: u32, scope: u32, el: u32, data: *mut c_void, size: *mut u32) -> i32;
    fn AudioUnitInitialize(unit: *mut c_void) -> i32;
    fn AudioUnitUninitialize(unit: *mut c_void) -> i32;
    fn AudioUnitReset(unit: *mut c_void, scope: u32, el: u32) -> i32;
    fn AudioUnitRender(
        unit: *mut c_void,
        flags: *mut u32,
        ts: *const TimeStamp,
        bus: u32,
        frames: u32,
        data: *mut BufferList,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
    fn CFStringGetCString(s: *const c_void, buf: *mut std::ffi::c_char, len: isize, encoding: u32) -> bool;
    fn CFPropertyListCreateData(
        alloc: *const c_void,
        plist: *const c_void,
        format: isize,
        options: usize,
        error: *mut *mut c_void,
    ) -> *const c_void;
    fn CFPropertyListCreateWithData(
        alloc: *const c_void,
        data: *const c_void,
        options: usize,
        format: *mut isize,
        error: *mut *mut c_void,
    ) -> *const c_void;
    fn CFDataCreate(alloc: *const c_void, bytes: *const u8, len: isize) -> *const c_void;
    fn CFDataGetLength(d: *const c_void) -> isize;
    fn CFDataGetBytePtr(d: *const c_void) -> *const u8;
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const TYPE_EFFECT: u32 = fourcc(b"aufx");
const TYPE_MUSIC_EFFECT: u32 = fourcc(b"aumf");
const FLAG_V3: u32 = 4;
const SCOPE_GLOBAL: u32 = 0;
const SCOPE_INPUT: u32 = 1;
const SCOPE_OUTPUT: u32 = 2;
const PROP_CLASS_INFO: u32 = 0;
const PROP_STREAM_FORMAT: u32 = 8;
const PROP_LATENCY: u32 = 12;
const PROP_MAX_FRAMES: u32 = 14;
const PROP_RENDER_CALLBACK: u32 = 23;
const LPCM: u32 = fourcc(b"lpcm");
/// Float, packed, non-interleaved.
const FLOAT_PLANAR: u32 = 1 | 8 | 32;
const UTF8: u32 = 0x0800_0100;
const BINARY_PLIST: isize = 200;
/// The out-of-process unit went away (crashed).
const ERR_INVALIDATED: i32 = -66749;
/// Frames per render call (blocks are split into these).
const MAX_FRAMES: usize = 4096;

fn cf_string(s: *const c_void) -> String {
    let mut buf = vec![0u8; 512];
    if s.is_null() || !unsafe { CFStringGetCString(s, buf.as_mut_ptr() as *mut std::ffi::c_char, buf.len() as isize, UTF8) } {
        return String::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

fn au_id(d: &Desc) -> String {
    format!("au:{:08x}:{:08x}:{:08x}", d.ty, d.sub, d.manu)
}

fn parse_au_id(id: &str) -> Option<Desc> {
    let mut parts = id.strip_prefix("au:")?.split(':').map(|p| u32::from_str_radix(p, 16));
    let (ty, sub, manu) = (parts.next()?.ok()?, parts.next()?.ok()?, parts.next()?.ok()?);
    Some(Desc { ty, sub, manu, ..Default::default() })
}

/// The installed Audio Unit effects (v2 and v3), by vendor and name.
pub fn scan_audio_units() -> Vec<PluginInfo> {
    let mut out: Vec<PluginInfo> = Vec::new();
    for ty in [TYPE_EFFECT, TYPE_MUSIC_EFFECT] {
        let want = Desc { ty, ..Default::default() };
        let mut c = std::ptr::null_mut();
        loop {
            c = unsafe { AudioComponentFindNext(c, &want) };
            if c.is_null() {
                break;
            }
            let mut d = Desc::default();
            if unsafe { AudioComponentGetDescription(c, &mut d) } != 0 {
                continue;
            }
            let mut name_ref = std::ptr::null();
            let full = if unsafe { AudioComponentCopyName(c, &mut name_ref) } == 0 {
                let s = cf_string(name_ref);
                unsafe { CFRelease(name_ref) };
                s
            } else {
                String::new()
            };
            let (vendor, name) = match full.split_once(": ") {
                Some((v, n)) => (v.trim().to_string(), n.trim().to_string()),
                None => (String::new(), full.trim().to_string()),
            };
            let mut v = 0u32;
            unsafe { AudioComponentGetVersion(c, &mut v) };
            let version = format!("{}.{}.{}", v >> 16, (v >> 8) & 0xff, v & 0xff);
            let id = au_id(&d);
            if name.is_empty() || out.iter().any(|p| p.id == id) {
                continue;
            }
            out.push(PluginInfo { format: "au", id, name, vendor, version, v3: d.flags & FLAG_V3 != 0 });
        }
    }
    out.sort_by_key(|p| (p.vendor.to_lowercase(), p.name.to_lowercase()));
    out
}

// ------------------------------------------------------------------ an instance

struct Io {
    input: [Vec<f32>; 2],
    frames: u32,
    sample_time: f64,
    list: BufferList,
}

/// A loaded Audio Unit, rendering stereo (or mono) float blocks.
pub struct AudioUnitPlugin {
    unit: *mut c_void,
    pub info: PluginInfo,
    channels: u32,
    busy: AtomicBool,
    crashed: AtomicBool,
    latency: AtomicUsize,
    io: UnsafeCell<Io>,
}

// The unit is used from the audio thread (render, guarded by `busy`) and from others (state,
// editor), which Audio Units allow.
unsafe impl Send for AudioUnitPlugin {}
unsafe impl Sync for AudioUnitPlugin {}

/// Feeds the unit its input: the block the host is rendering.
unsafe extern "C" fn render_input(
    refcon: *mut c_void,
    _flags: *mut u32,
    _ts: *const TimeStamp,
    _bus: u32,
    frames: u32,
    data: *mut BufferList,
) -> i32 {
    let io = unsafe { &mut *(refcon as *mut Io) };
    if data.is_null() {
        return 0;
    }
    let n = unsafe { (*data).n } as usize;
    let bufs = unsafe { std::ptr::addr_of_mut!((*data).bufs) as *mut Buffer };
    let frames = (frames as usize).min(io.frames as usize);
    for i in 0..n {
        let b = unsafe { &mut *bufs.add(i) };
        let src = &io.input[i.min(1)];
        if b.data.is_null() {
            b.data = src.as_ptr() as *mut c_void;
        } else {
            unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), b.data as *mut f32, frames) };
        }
        b.size = (frames * 4) as u32;
    }
    0
}

impl AudioUnitPlugin {
    /// Sets the unit up for `channels` float planar channels at `rate` and initialises it.
    fn configure(unit: *mut c_void, rate: f64, channels: u32) -> Result<(), i32> {
        let f = Asbd {
            rate,
            format: LPCM,
            flags: FLOAT_PLANAR,
            bytes_per_packet: 4,
            frames_per_packet: 1,
            bytes_per_frame: 4,
            channels,
            bits: 32,
            reserved: 0,
        };
        let size = std::mem::size_of::<Asbd>() as u32;
        for scope in [SCOPE_INPUT, SCOPE_OUTPUT] {
            let st = unsafe { AudioUnitSetProperty(unit, PROP_STREAM_FORMAT, scope, 0, &f as *const _ as *const c_void, size) };
            if st != 0 {
                return Err(st);
            }
        }
        let max = MAX_FRAMES as u32;
        unsafe { AudioUnitSetProperty(unit, PROP_MAX_FRAMES, SCOPE_GLOBAL, 0, &max as *const _ as *const c_void, 4) };
        Ok(())
    }

    /// Takes an instantiated unit, configures it and starts it.
    fn new(unit: *mut c_void, info: PluginInfo, rate: f64) -> Result<Arc<Self>, String> {
        let mut channels = 2;
        if Self::configure(unit, rate, 2).is_err() {
            channels = 1;
            Self::configure(unit, rate, 1).map_err(|st| format!("it doesn't take stereo or mono audio ({st})"))?;
        }
        let p = Arc::new(Self {
            unit,
            info,
            channels,
            busy: AtomicBool::new(false),
            crashed: AtomicBool::new(false),
            latency: AtomicUsize::new(0),
            io: UnsafeCell::new(Io {
                input: [vec![0.0; MAX_FRAMES], vec![0.0; MAX_FRAMES]],
                frames: 0,
                sample_time: 0.0,
                list: BufferList {
                    n: channels,
                    bufs: [Buffer { channels: 1, size: 0, data: std::ptr::null_mut() }; 2],
                },
            }),
        });
        let cb = RenderCallback { proc_: render_input, refcon: p.io.get() as *mut c_void };
        let size = std::mem::size_of::<RenderCallback>() as u32;
        let st = unsafe {
            AudioUnitSetProperty(unit, PROP_RENDER_CALLBACK, SCOPE_INPUT, 0, &cb as *const _ as *const c_void, size)
        };
        if st != 0 {
            return Err(format!("it takes no input ({st})"));
        }
        let st = unsafe { AudioUnitInitialize(unit) };
        if st != 0 {
            return Err(format!("it didn't start ({st})"));
        }
        p.read_latency(rate);
        Ok(p)
    }

    fn read_latency(&self, rate: f64) {
        let mut secs = 0f64;
        let mut size = 8u32;
        let st = unsafe {
            AudioUnitGetProperty(self.unit, PROP_LATENCY, SCOPE_GLOBAL, 0, &mut secs as *mut _ as *mut c_void, &mut size)
        };
        if st == 0 && secs.is_finite() && secs > 0.0 {
            self.latency.store((secs * rate).round() as usize, Ordering::Relaxed);
        }
    }

    /// The AudioUnit handle (for its editor).
    pub fn raw(&self) -> *mut c_void {
        self.unit
    }

    /// Whether its process went away (out-of-process units).
    pub fn crashed(&self) -> bool {
        self.crashed.load(Ordering::Relaxed)
    }

    /// Its state (the unit's class info as a binary plist), base64.
    pub fn state(&self) -> Option<String> {
        let mut plist: *const c_void = std::ptr::null();
        let mut size = std::mem::size_of::<*const c_void>() as u32;
        let st = unsafe {
            AudioUnitGetProperty(self.unit, PROP_CLASS_INFO, SCOPE_GLOBAL, 0, &mut plist as *mut _ as *mut c_void, &mut size)
        };
        if st != 0 || plist.is_null() {
            return None;
        }
        let data = unsafe { CFPropertyListCreateData(std::ptr::null(), plist, BINARY_PLIST, 0, std::ptr::null_mut()) };
        unsafe { CFRelease(plist) };
        if data.is_null() {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(CFDataGetBytePtr(data), CFDataGetLength(data) as usize) };
        let out = base64::engine::general_purpose::STANDARD.encode(bytes);
        unsafe { CFRelease(data) };
        Some(out)
    }

    /// Restores a state from [`Self::state`].
    pub fn set_state(&self, state: &str) -> bool {
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(state) else { return false };
        let data = unsafe { CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize) };
        if data.is_null() {
            return false;
        }
        let plist = unsafe { CFPropertyListCreateWithData(std::ptr::null(), data, 0, std::ptr::null_mut(), std::ptr::null_mut()) };
        unsafe { CFRelease(data) };
        if plist.is_null() {
            return false;
        }
        let size = std::mem::size_of::<*const c_void>() as u32;
        let st = unsafe {
            AudioUnitSetProperty(self.unit, PROP_CLASS_INFO, SCOPE_GLOBAL, 0, &plist as *const _ as *const c_void, size)
        };
        unsafe { CFRelease(plist) };
        st == 0
    }

    fn render(&self, l: &mut [f32], r: &mut [f32]) -> bool {
        let io = unsafe { &mut *self.io.get() };
        let n = l.len();
        if self.channels == 1 {
            for i in 0..n {
                io.input[0][i] = 0.5 * (l[i] + r[i]);
            }
        } else {
            io.input[0][..n].copy_from_slice(l);
            io.input[1][..n].copy_from_slice(r);
        }
        io.frames = n as u32;
        let (lp, rp) = (l.as_mut_ptr(), r.as_mut_ptr());
        io.list.n = self.channels;
        io.list.bufs[0] = Buffer { channels: 1, size: (n * 4) as u32, data: lp as *mut c_void };
        io.list.bufs[1] = Buffer { channels: 1, size: (n * 4) as u32, data: rp as *mut c_void };
        let mut flags = 0u32;
        let ts = TimeStamp { sample_time: io.sample_time, flags: 1, ..Default::default() };
        let st = unsafe { AudioUnitRender(self.unit, &mut flags, &ts, 0, n as u32, &mut io.list) };
        io.sample_time += n as f64;
        if st != 0 {
            if st == ERR_INVALIDATED {
                self.crashed.store(true, Ordering::Relaxed);
            }
            // pass the input through
            if self.channels == 2 {
                l.copy_from_slice(&io.input[0][..n]);
                r.copy_from_slice(&io.input[1][..n]);
            }
            return false;
        }
        // a unit may answer with its own buffers
        let out = io.list.bufs;
        if out[0].data != lp as *mut c_void && !out[0].data.is_null() {
            unsafe { std::ptr::copy_nonoverlapping(out[0].data as *const f32, lp, n) };
        }
        if self.channels == 1 {
            r.copy_from_slice(l);
        } else if out[1].data != rp as *mut c_void && !out[1].data.is_null() {
            unsafe { std::ptr::copy_nonoverlapping(out[1].data as *const f32, rp, n) };
        }
        true
    }
}

impl Plugin for AudioUnitPlugin {
    fn process(&self, l: &mut [f32], r: &mut [f32]) -> bool {
        if self.crashed() || self.busy.swap(true, Ordering::Acquire) {
            return false;
        }
        let mut ok = true;
        let n = l.len().min(r.len());
        let mut at = 0;
        while at < n {
            let end = (at + MAX_FRAMES).min(n);
            ok &= self.render(&mut l[at..end], &mut r[at..end]);
            at = end;
        }
        self.busy.store(false, Ordering::Release);
        ok
    }

    fn latency(&self) -> usize {
        self.latency.load(Ordering::Relaxed)
    }

    fn reset(&self) {
        unsafe { AudioUnitReset(self.unit, SCOPE_GLOBAL, 0) };
    }
}

impl Drop for AudioUnitPlugin {
    fn drop(&mut self) {
        unsafe {
            AudioUnitUninitialize(self.unit);
            AudioComponentInstanceDispose(self.unit);
        }
    }
}

/// Instantiates an Audio Unit (asynchronously: AUv3 units may load out of process) and calls
/// `done` with it, configured for `rate`, on some thread.
pub fn load_audio_unit(
    info: &PluginInfo,
    rate: f64,
    done: impl FnOnce(Result<Arc<AudioUnitPlugin>, String>) + Send + 'static,
) {
    let Some(want) = parse_au_id(&info.id) else {
        return done(Err("not an Audio Unit".into()));
    };
    // zero fields are wildcards to the search: take only the exact unit
    let c = unsafe { AudioComponentFindNext(std::ptr::null_mut(), &want) };
    let mut found = Desc::default();
    if c.is_null()
        || unsafe { AudioComponentGetDescription(c, &mut found) } != 0
        || (found.ty, found.sub, found.manu) != (want.ty, want.sub, want.manu)
    {
        return done(Err("not installed".into()));
    }
    let info = info.clone();
    let done = Mutex::new(Some(done));
    let block = block2::RcBlock::new(move |unit: *mut c_void, status: i32| {
        let Some(done) = done.lock().take() else { return };
        if status != 0 || unit.is_null() {
            return done(Err(format!("it didn't load ({status})")));
        }
        let result = AudioUnitPlugin::new(unit, info.clone(), rate);
        if result.is_err() {
            unsafe { AudioComponentInstanceDispose(unit) };
        }
        done(result)
    });
    unsafe { AudioComponentInstantiate(c, 0, &block) };
}

// ------------------------------------------------------------------ the project's inserts

/// Where a plugin insert stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    Loading,
    Ready { latency: usize },
    Failed(String),
    /// Not a plugin insert the host knows.
    None,
}

enum Slot {
    Loading,
    Ready(Arc<AudioUnitPlugin>),
    Failed(String),
}

struct Entry {
    id: String,
    slot: Slot,
    /// The state last given to (or read from) the instance.
    applied: Option<String>,
}

static ENTRIES: LazyLock<Mutex<HashMap<Id, Entry>>> = LazyLock::new(|| Mutex::new(HashMap::new()));
static GENERATION: AtomicU64 = AtomicU64::new(0);
static SEEN: AtomicU64 = AtomicU64::new(0);
static SAMPLE_RATE: AtomicU32 = AtomicU32::new(48_000);

/// The rate plugins are set up for (the engine's).
pub fn set_sample_rate(rate: u32) {
    SAMPLE_RATE.store(rate, Ordering::Relaxed);
}

/// Whether a plugin finished loading (or failed) since the last call: the mixer should be
/// played again to pick it up.
pub fn changed() -> bool {
    let g = GENERATION.load(Ordering::Acquire);
    SEEN.swap(g, Ordering::AcqRel) != g
}

fn setting<'a>(e: &'a Effect, key: &str) -> Option<&'a str> {
    e.settings.get(key).and_then(serde_json::Value::as_str)
}

/// The running instance for a plugin insert, starting it if needed (`None` until it's ready).
/// A changed "state" setting (undo, a preset) is given to the running instance.
pub fn instance(e: &Effect) -> Option<Arc<dyn Plugin>> {
    let (format, id) = (setting(e, "format")?, setting(e, "id")?);
    if format != "au" {
        return None;
    }
    let state = setting(e, "state").map(str::to_string);
    {
        let mut entries = ENTRIES.lock();
        if let Some(entry) = entries.get_mut(&e.id)
            && entry.id == id
        {
            return match &entry.slot {
                Slot::Ready(p) => {
                    if let Some(s) = &state
                        && entry.applied.as_ref() != Some(s)
                    {
                        p.set_state(s);
                        entry.applied = Some(s.clone());
                    }
                    Some(p.clone() as Arc<dyn Plugin>)
                }
                Slot::Loading | Slot::Failed(_) => None,
            };
        }
        entries.insert(e.id, Entry { id: id.to_string(), slot: Slot::Loading, applied: None });
    }
    // outside the lock: the unit may finish loading right inside this call
    let info = PluginInfo {
        format: "au",
        id: id.to_string(),
        name: setting(e, "name").unwrap_or("Plugin").to_string(),
        vendor: setting(e, "vendor").unwrap_or("").to_string(),
        version: String::new(),
        v3: false,
    };
    let effect = e.id;
    let rate = SAMPLE_RATE.load(Ordering::Relaxed) as f64;
    let (pid, pname) = (info.id.clone(), info.name.clone());
    load_audio_unit(&info, rate, move |result| {
        let mut entries = ENTRIES.lock();
        let Some(entry) = entries.get_mut(&effect).filter(|en| en.id == pid) else { return };
        match result {
            Ok(p) => {
                if let Some(s) = &state {
                    p.set_state(s);
                }
                entry.applied = state;
                tracing::info!(plugin = %pname, latency = p.latency(), "plugin loaded");
                entry.slot = Slot::Ready(p);
            }
            Err(why) => {
                tracing::warn!(plugin = %pname, %why, "plugin didn't load");
                entry.slot = Slot::Failed(why);
            }
        }
        GENERATION.fetch_add(1, Ordering::AcqRel);
    });
    None
}

/// Where an insert's plugin stands.
pub fn status(effect: Id) -> Status {
    match ENTRIES.lock().get(&effect).map(|e| &e.slot) {
        Some(Slot::Loading) => Status::Loading,
        Some(Slot::Ready(p)) if p.crashed() => Status::Failed("it stopped working".into()),
        Some(Slot::Ready(p)) => Status::Ready { latency: p.latency() },
        Some(Slot::Failed(why)) => Status::Failed(why.clone()),
        None => Status::None,
    }
}

/// An insert's loaded unit (its editor, its state).
pub fn unit(effect: Id) -> Option<Arc<AudioUnitPlugin>> {
    match ENTRIES.lock().get(&effect).map(|e| &e.slot) {
        Some(Slot::Ready(p)) => Some(p.clone()),
        _ => None,
    }
}

/// Reads an insert's current state from its instance (what its editor changed). Remembered as
/// applied, so writing it into the project doesn't give it back to the instance.
pub fn take_state(effect: Id) -> Option<String> {
    let mut entries = ENTRIES.lock();
    let entry = entries.get_mut(&effect)?;
    let Slot::Ready(p) = &entry.slot else { return None };
    let s = p.state()?;
    entry.applied = Some(s.clone());
    Some(s)
}

/// Lets go of instances whose inserts are gone.
pub fn retain(keep: &HashSet<Id>) {
    let gone: Vec<Entry> = {
        let mut entries = ENTRIES.lock();
        let ids: Vec<Id> = entries
            .keys()
            .filter(|id| !keep.contains(id))
            .copied()
            .collect();
        ids.into_iter().filter_map(|id| entries.remove(&id)).collect()
    };
    // disposed outside the lock
    drop(gone);
}

/// The plugin inserts in a project (tracks, buses, mixes, master).
pub fn inserts(project: &lightair_core::Project) -> HashSet<Id> {
    let a = &project.audio;
    project
        .tracks
        .iter()
        .flat_map(|t| t.effects.iter())
        .chain(a.buses.iter().flat_map(|b| b.effects.iter()))
        .chain(a.mixes.iter().flat_map(|m| m.effects.iter()))
        .chain(a.master.effects.iter())
        .filter(|e| e.kind == lightair_core::catalog::PLUGIN_KIND)
        .map(|e| e.id)
        .collect()
}
