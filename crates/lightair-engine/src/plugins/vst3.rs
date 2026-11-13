//! VST3 hosting: bundles in the system and user VST3 folders, loaded with CoreFoundation, one
//! component and its controller per insert, processed in place as 32-bit float stereo.
//!
//! VST3 (and the JUCE wrappers most plugins use) expect their component and controller made,
//! edited and torn down on the main thread: those calls go through [`super::on_main`]. Only
//! `process` runs on the audio thread.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use base64::Engine as _;
use lightair_audio::dsp::Plugin;
use parking_lot::Mutex;
use vst3::Steinberg::Vst::{
    BusDirections_::{kInput, kOutput},
    MediaTypes_::kAudio,
    ProcessContext_::StatesAndFlags_::{kContTimeValid, kTempoValid, kTimeSigValid},
    ProcessModes_::kRealtime,
    RestartFlags_::kLatencyChanged,
    SymbolicSampleSizes_::kSample32,
    *,
};
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComWrapper, Interface};

use super::PluginInfo;

// ------------------------------------------------------------------ CoreFoundation

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFURLCreateFromFileSystemRepresentation(alloc: *const c_void, buf: *const u8, len: isize, dir: bool) -> *const c_void;
    fn CFBundleCreate(alloc: *const c_void, url: *const c_void) -> *const c_void;
    fn CFBundleLoadExecutable(bundle: *const c_void) -> bool;
    fn CFBundleGetFunctionPointerForName(bundle: *const c_void, name: *const c_void) -> *mut c_void;
    fn CFStringCreateWithCString(alloc: *const c_void, s: *const c_char, encoding: u32) -> *const c_void;
    fn CFRelease(cf: *const c_void);
}

const UTF8: u32 = 0x0800_0100;
const AUDIO_EFFECT_CLASS: &str = "Audio Module Class";
const MAX_FRAMES: usize = 4096;
const MAX_CHANGES: usize = 64;

fn ok(r: tresult) -> bool {
    r == kResultOk
}

fn text(chars: &[c_char]) -> String {
    let end = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
    let bytes: Vec<u8> = chars[..end].iter().map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

fn cid_hex(cid: &TUID) -> String {
    cid.iter().map(|b| format!("{:02X}", *b as u8)).collect()
}

fn parse_cid(hex: &str) -> Option<TUID> {
    if hex.len() != 32 {
        return None;
    }
    let mut out: TUID = [0; 16];
    for (i, o) in out.iter_mut().enumerate() {
        *o = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()? as c_char;
    }
    Some(out)
}

/// The VST3 id of a class: "vst3:<CID>".
pub fn plugin_id(cid: &str) -> String {
    format!("vst3:{}", cid.to_uppercase())
}

// ------------------------------------------------------------------ modules

/// A loaded bundle and its factory. Never unloaded: unloading plugin code mid-session is a
/// classic crash.
struct Module {
    factory: ComPtr<IPluginFactory>,
}

unsafe impl Send for Module {}
unsafe impl Sync for Module {}

static MODULES: LazyLock<Mutex<HashMap<PathBuf, Arc<Module>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

fn module(path: &Path) -> Result<Arc<Module>, String> {
    if let Some(m) = MODULES.lock().get(path) {
        return Ok(m.clone());
    }
    let bytes = path.as_os_str().as_encoded_bytes();
    unsafe {
        let url = CFURLCreateFromFileSystemRepresentation(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize, true);
        if url.is_null() {
            return Err("bad path".into());
        }
        let bundle = CFBundleCreate(std::ptr::null(), url);
        CFRelease(url);
        if bundle.is_null() {
            return Err("not a bundle".into());
        }
        if !CFBundleLoadExecutable(bundle) {
            return Err("its code didn't load".into());
        }
        let sym = |name: &CStr| {
            let s = CFStringCreateWithCString(std::ptr::null(), name.as_ptr(), UTF8);
            let f = CFBundleGetFunctionPointerForName(bundle, s);
            CFRelease(s);
            f
        };
        let entry = sym(c"bundleEntry");
        if !entry.is_null() {
            let entry: extern "C" fn(*const c_void) -> bool = std::mem::transmute(entry);
            if !entry(bundle) {
                return Err("it didn't start".into());
            }
        }
        let get = sym(c"GetPluginFactory");
        if get.is_null() {
            return Err("not a VST3 plugin".into());
        }
        let get: extern "system" fn() -> *mut IPluginFactory = std::mem::transmute(get);
        // the bundle stays loaded (and referenced) for the session
        let factory = ComPtr::from_raw(get()).ok_or("no plugin factory")?;
        let m = Arc::new(Module { factory });
        MODULES.lock().insert(path.to_path_buf(), m.clone());
        Ok(m)
    }
}

// ------------------------------------------------------------------ scanning

/// The folders VST3 plugins are installed in.
pub fn folders() -> Vec<PathBuf> {
    let mut v = vec![PathBuf::from("/Library/Audio/Plug-Ins/VST3")];
    if let Some(home) = std::env::var_os("HOME") {
        v.push(PathBuf::from(home).join("Library/Audio/Plug-Ins/VST3"));
    }
    v
}

/// Every .vst3 bundle in the folders (and one level of subfolders).
pub fn bundles() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let is_bundle = |p: &Path| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("vst3"));
    for dir in folders() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if is_bundle(&p) {
                out.push(p);
            } else if p.is_dir()
                && let Ok(sub) = std::fs::read_dir(&p)
            {
                out.extend(sub.flatten().map(|e| e.path()).filter(|p| is_bundle(p)));
            }
        }
    }
    out.sort();
    out
}

/// JSON5-ish (trailing commas, as moduleinfo.json files have) to JSON.
fn strip_trailing_commas(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let (mut in_str, mut escaped) = (false, false);
    let chars: Vec<char> = s.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if in_str {
            out.push(c);
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_str = false,
                _ => {}
            }
            continue;
        }
        if c == '"' {
            in_str = true;
        } else if c == ',' && chars[i + 1..].iter().find(|c| !c.is_whitespace()).is_some_and(|n| *n == '}' || *n == ']') {
            continue;
        }
        out.push(c);
    }
    out
}

/// A bundle's effects from its moduleinfo.json (no code loaded); `None` without one.
pub fn from_module_info(bundle: &Path) -> Option<Vec<PluginInfo>> {
    let raw = std::fs::read_to_string(bundle.join("Contents/Resources/moduleinfo.json")).ok()?;
    let v: serde_json::Value = serde_json::from_str(&strip_trailing_commas(&raw)).ok()?;
    let factory_vendor = v["Factory Info"]["Vendor"].as_str().unwrap_or("");
    let classes = v["Classes"].as_array()?;
    Some(
        classes
            .iter()
            .filter(|c| c["Category"].as_str() == Some(AUDIO_EFFECT_CLASS))
            .filter(|c| {
                c["Sub Categories"]
                    .as_array()
                    .is_some_and(|s| s.iter().any(|x| x.as_str().is_some_and(|x| x.starts_with("Fx"))))
            })
            .filter_map(|c| {
                Some(PluginInfo {
                    format: "vst3".into(),
                    id: plugin_id(c["CID"].as_str()?),
                    name: c["Name"].as_str()?.to_string(),
                    vendor: c["Vendor"].as_str().filter(|s| !s.is_empty()).unwrap_or(factory_vendor).to_string(),
                    version: c["Version"].as_str().unwrap_or("").to_string(),
                    v3: false,
                    path: bundle.to_string_lossy().into_owned(),
                })
            })
            .collect(),
    )
}

/// A bundle's effects, by loading it and asking its factory. Runs the plugin's code: the app
/// does this in a helper process, so a plugin that crashes on load can't take Lightair down.
pub fn probe(bundle: &Path) -> Result<Vec<PluginInfo>, String> {
    let m = module(bundle)?;
    let f = &m.factory;
    let f2 = f.cast::<IPluginFactory2>();
    let mut factory_info: PFactoryInfo = unsafe { std::mem::zeroed() };
    unsafe { f.getFactoryInfo(&mut factory_info) };
    let factory_vendor = text(&factory_info.vendor);
    let mut out = Vec::new();
    for i in 0..unsafe { f.countClasses() } {
        let (cid, category, name, sub, vendor, version) = match &f2 {
            Some(f2) => {
                let mut c: PClassInfo2 = unsafe { std::mem::zeroed() };
                if !ok(unsafe { f2.getClassInfo2(i, &mut c) }) {
                    continue;
                }
                (c.cid, text(&c.category), text(&c.name), text(&c.subCategories), text(&c.vendor), text(&c.version))
            }
            None => {
                let mut c: PClassInfo = unsafe { std::mem::zeroed() };
                if !ok(unsafe { f.getClassInfo(i, &mut c) }) {
                    continue;
                }
                // no subcategories to tell effects from instruments: taken as an effect
                (c.cid, text(&c.category), text(&c.name), "Fx".into(), String::new(), String::new())
            }
        };
        if category != AUDIO_EFFECT_CLASS || !sub.split('|').any(|s| s.starts_with("Fx")) {
            continue;
        }
        out.push(PluginInfo {
            format: "vst3".into(),
            id: plugin_id(&cid_hex(&cid)),
            name,
            vendor: if vendor.is_empty() { factory_vendor.clone() } else { vendor },
            version,
            v3: false,
            path: bundle.to_string_lossy().into_owned(),
        });
    }
    Ok(out)
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Cache {
    bundles: HashMap<String, (u64, Vec<PluginInfo>)>,
}

fn mtime(p: &Path) -> u64 {
    let m = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    m(&p.join("Contents/Info.plist"))
        .or_else(|| m(p))
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs())
}

/// The installed VST3 effects. Bundles with a moduleinfo.json are read without loading; others
/// go to `probe` (loading the plugin, best in another process), remembered in `cache` (a JSON
/// file) until the bundle changes.
pub fn scan(cache: Option<&Path>, probe: &dyn Fn(&Path) -> Result<Vec<PluginInfo>, String>) -> Vec<PluginInfo> {
    let mut known: Cache =
        cache.and_then(|c| std::fs::read(c).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let mut fresh = Cache::default();
    let mut out = Vec::new();
    for b in bundles() {
        let key = b.to_string_lossy().into_owned();
        let stamp = mtime(&b);
        let found = match known.bundles.remove(&key) {
            Some((t, list)) if t == stamp => list,
            _ => from_module_info(&b).unwrap_or_else(|| {
                probe(&b).unwrap_or_else(|why| {
                    tracing::warn!(bundle = %key, %why, "VST3 bundle skipped");
                    Vec::new()
                })
            }),
        };
        out.extend(found.iter().cloned());
        fresh.bundles.insert(key, (stamp, found));
    }
    if let Some(c) = cache
        && let Ok(json) = serde_json::to_vec(&fresh)
    {
        let _ = std::fs::create_dir_all(c.parent().unwrap_or(Path::new(".")));
        let _ = std::fs::write(c, json);
    }
    out
}

// ------------------------------------------------------------------ host objects

struct HostApp;

impl Class for HostApp {
    type Interfaces = (IHostApplication,);
}

fn copy_utf16(src: &str, dst: &mut [TChar]) {
    let mut n = 0;
    for (s, d) in src.encode_utf16().zip(dst.iter_mut()) {
        *d = s as TChar;
        n += 1;
    }
    if let Some(end) = dst.get_mut(n.min(dst.len() - 1)) {
        *end = 0;
    }
}

impl IHostApplicationTrait for HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        copy_utf16("Lightair", unsafe { &mut *name });
        kResultOk
    }

    unsafe fn createInstance(&self, cid: *mut TUID, iid: *mut TUID, obj: *mut *mut c_void) -> tresult {
        let wants = |want: &vst3::com_scrape_types::Guid| unsafe {
            let c = std::slice::from_raw_parts(cid as *const u8, 16);
            let i = std::slice::from_raw_parts(iid as *const u8, 16);
            c == want && i == want
        };
        unsafe {
            if wants(&IMessage::IID) {
                let m = ComWrapper::new(Message::default());
                *obj = m.to_com_ptr::<IMessage>().map_or(std::ptr::null_mut(), |p| p.into_raw() as *mut c_void);
                return kResultOk;
            }
            if wants(&IAttributeList::IID) {
                let a = ComWrapper::new(Attributes::default());
                *obj = a.to_com_ptr::<IAttributeList>().map_or(std::ptr::null_mut(), |p| p.into_raw() as *mut c_void);
                return kResultOk;
            }
            *obj = std::ptr::null_mut();
        }
        kResultFalse
    }
}

#[derive(Default)]
struct Message {
    id: RefCell<std::ffi::CString>,
    attributes: RefCell<Option<ComWrapper<Attributes>>>,
}

impl Class for Message {
    type Interfaces = (IMessage,);
}

impl IMessageTrait for Message {
    unsafe fn getMessageID(&self) -> FIDString {
        self.id.borrow().as_ptr()
    }
    unsafe fn setMessageID(&self, id: FIDString) {
        if !id.is_null() {
            *self.id.borrow_mut() = unsafe { CStr::from_ptr(id) }.to_owned();
        }
    }
    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        let mut a = self.attributes.borrow_mut();
        let a = a.get_or_insert_with(|| ComWrapper::new(Attributes::default()));
        a.as_com_ref::<IAttributeList>().map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }
}

#[derive(Clone)]
enum Attr {
    Int(i64),
    Float(f64),
    Text(Vec<TChar>),
    Bytes(Vec<u8>),
}

#[derive(Default)]
struct Attributes {
    map: RefCell<HashMap<Vec<u8>, Attr>>,
}

impl Class for Attributes {
    type Interfaces = (IAttributeList,);
}

type AttrID = *const c_char;

fn key(id: AttrID) -> Vec<u8> {
    if id.is_null() { Vec::new() } else { unsafe { CStr::from_ptr(id) }.to_bytes().to_vec() }
}

impl IAttributeListTrait for Attributes {
    unsafe fn setInt(&self, id: AttrID, value: int64) -> tresult {
        self.map.borrow_mut().insert(key(id), Attr::Int(value));
        kResultOk
    }
    unsafe fn getInt(&self, id: AttrID, value: *mut int64) -> tresult {
        match self.map.borrow().get(&key(id)) {
            Some(Attr::Int(v)) => {
                unsafe { *value = *v };
                kResultOk
            }
            _ => kResultFalse,
        }
    }
    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        self.map.borrow_mut().insert(key(id), Attr::Float(value));
        kResultOk
    }
    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        match self.map.borrow().get(&key(id)) {
            Some(Attr::Float(v)) => {
                unsafe { *value = *v };
                kResultOk
            }
            _ => kResultFalse,
        }
    }
    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        let mut v = Vec::new();
        let mut i = 0;
        while !string.is_null() && unsafe { *string.add(i) } != 0 {
            v.push(unsafe { *string.add(i) });
            i += 1;
        }
        self.map.borrow_mut().insert(key(id), Attr::Text(v));
        kResultOk
    }
    unsafe fn getString(&self, id: AttrID, string: *mut TChar, size_in_bytes: uint32) -> tresult {
        match self.map.borrow().get(&key(id)) {
            Some(Attr::Text(v)) => {
                let cap = size_in_bytes as usize / 2;
                if cap == 0 {
                    return kResultFalse;
                }
                let n = v.len().min(cap - 1);
                unsafe {
                    std::ptr::copy_nonoverlapping(v.as_ptr(), string, n);
                    *string.add(n) = 0;
                }
                kResultOk
            }
            _ => kResultFalse,
        }
    }
    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, size_in_bytes: uint32) -> tresult {
        let bytes = if data.is_null() {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(data as *const u8, size_in_bytes as usize) }.to_vec()
        };
        self.map.borrow_mut().insert(key(id), Attr::Bytes(bytes));
        kResultOk
    }
    unsafe fn getBinary(&self, id: AttrID, data: *mut *const c_void, size_in_bytes: *mut uint32) -> tresult {
        match self.map.borrow().get(&key(id)) {
            Some(Attr::Bytes(v)) => {
                // the list owns the bytes for its life (stable: not moved while borrowed here)
                unsafe {
                    *data = v.as_ptr() as *const c_void;
                    *size_in_bytes = v.len() as uint32;
                }
                kResultOk
            }
            _ => kResultFalse,
        }
    }
}

/// Edits from the plugin's editor, on their way to its processor.
#[derive(Default)]
struct Handler {
    pending: Mutex<Vec<(ParamID, ParamValue)>>,
    latency_changed: AtomicBool,
}

impl Class for Handler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for Handler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }
    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        let mut p = self.pending.lock();
        match p.iter_mut().find(|(i, _)| *i == id) {
            Some(slot) => slot.1 = value,
            None => p.push((id, value)),
        }
        kResultOk
    }
    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }
    unsafe fn restartComponent(&self, flags: int32) -> tresult {
        if flags & kLatencyChanged as int32 != 0 {
            self.latency_changed.store(true, Ordering::Relaxed);
        }
        kResultOk
    }
}

/// One parameter's change in a block (a single point at its start).
#[derive(Default)]
struct Queue {
    id: Cell<ParamID>,
    value: Cell<ParamValue>,
}

impl Class for Queue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for Queue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id.get()
    }
    unsafe fn getPointCount(&self) -> int32 {
        1
    }
    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        if index != 0 {
            return kResultFalse;
        }
        unsafe {
            *offset = 0;
            *value = self.value.get();
        }
        kResultOk
    }
    unsafe fn addPoint(&self, _offset: int32, _value: ParamValue, _index: *mut int32) -> tresult {
        kResultFalse
    }
}

/// A block's parameter changes (preallocated: filled on the audio thread).
struct Changes {
    queues: Vec<ComWrapper<Queue>>,
    count: Cell<usize>,
}

impl Class for Changes {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for Changes {
    unsafe fn getParameterCount(&self) -> int32 {
        self.count.get() as int32
    }
    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        if index < 0 || index as usize >= self.count.get() {
            return std::ptr::null_mut();
        }
        self.queues[index as usize].as_com_ref::<IParamValueQueue>().map_or(std::ptr::null_mut(), |r| r.as_ptr())
    }
    unsafe fn addParameterData(&self, _id: *const ParamID, _index: *mut int32) -> *mut IParamValueQueue {
        std::ptr::null_mut()
    }
}

/// A memory stream for states.
#[derive(Default)]
struct Stream {
    data: RefCell<Vec<u8>>,
    pos: Cell<usize>,
}

impl Class for Stream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for Stream {
    unsafe fn read(&self, buffer: *mut c_void, n: int32, read: *mut int32) -> tresult {
        let data = self.data.borrow();
        let pos = self.pos.get().min(data.len());
        let n = (n.max(0) as usize).min(data.len() - pos);
        unsafe { std::ptr::copy_nonoverlapping(data[pos..].as_ptr(), buffer as *mut u8, n) };
        self.pos.set(pos + n);
        if !read.is_null() {
            unsafe { *read = n as int32 };
        }
        kResultOk
    }
    unsafe fn write(&self, buffer: *mut c_void, n: int32, written: *mut int32) -> tresult {
        let n = n.max(0) as usize;
        let src = unsafe { std::slice::from_raw_parts(buffer as *const u8, n) };
        let mut data = self.data.borrow_mut();
        let pos = self.pos.get();
        if data.len() < pos + n {
            data.resize(pos + n, 0);
        }
        data[pos..pos + n].copy_from_slice(src);
        self.pos.set(pos + n);
        if !written.is_null() {
            unsafe { *written = n as int32 };
        }
        kResultOk
    }
    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        let len = self.data.borrow().len() as i64;
        let base = match mode {
            0 => 0,
            1 => self.pos.get() as i64,
            2 => len,
            _ => return kInvalidArgument,
        };
        let p = (base + pos).clamp(0, len);
        self.pos.set(p as usize);
        if !result.is_null() {
            unsafe { *result = p };
        }
        kResultOk
    }
    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if !pos.is_null() {
            unsafe { *pos = self.pos.get() as int64 };
        }
        kResultOk
    }
}

fn stream(bytes: Vec<u8>) -> ComWrapper<Stream> {
    ComWrapper::new(Stream { data: RefCell::new(bytes), pos: Cell::new(0) })
}

fn stream_ptr(s: &ComWrapper<Stream>) -> *mut IBStream {
    s.as_com_ref::<IBStream>().map_or(std::ptr::null_mut(), |r| r.as_ptr())
}

// ------------------------------------------------------------------ an instance

struct Io {
    input: [Vec<f32>; 2],
    in_ptrs: [*mut f32; 2],
    out_ptrs: [*mut f32; 2],
    context: ProcessContext,
}

/// A loaded VST3 effect.
pub struct Vst3Plugin {
    pub info: PluginInfo,
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    controller: Option<ComPtr<IEditController>>,
    /// The controller is its own object (initialised, connected and terminated apart).
    separate: bool,
    handler: ComWrapper<Handler>,
    _host: ComWrapper<HostApp>,
    changes: ComWrapper<Changes>,
    channels: (i32, i32),
    busy: AtomicBool,
    failed: AtomicBool,
    latency: AtomicUsize,
    io: std::cell::UnsafeCell<Io>,
}

// Main-thread calls (state, editor, teardown) go through `on_main`; `process` is guarded by `busy`.
unsafe impl Send for Vst3Plugin {}
unsafe impl Sync for Vst3Plugin {}

impl Vst3Plugin {
    /// Makes, connects and starts an instance at `rate`. Main thread.
    pub fn load(info: &PluginInfo, rate: f64) -> Result<Arc<Self>, String> {
        let cid = info.id.strip_prefix("vst3:").and_then(parse_cid).ok_or("not a VST3 id")?;
        let path = PathBuf::from(&info.path);
        let m = module(&path)?;
        let host = ComWrapper::new(HostApp);
        let host_ptr = host.as_com_ref::<IHostApplication>().ok_or("host")?.as_ptr() as *mut FUnknown;
        let create = |cid: &TUID, iid: &vst3::com_scrape_types::Guid| -> *mut c_void {
            let mut obj = std::ptr::null_mut();
            let r = unsafe { m.factory.createInstance(cid.as_ptr(), iid.as_ptr() as FIDString, &mut obj) };
            if ok(r) { obj } else { std::ptr::null_mut() }
        };
        let component = unsafe { ComPtr::from_raw(create(&cid, &IComponent::IID) as *mut IComponent) }
            .ok_or("its component didn't load")?;
        if !ok(unsafe { component.initialize(host_ptr) }) {
            return Err("its component didn't start".into());
        }
        let processor = component.cast::<IAudioProcessor>().ok_or("it doesn't process audio")?;

        // its controller: the component itself, or a class of its own
        let (controller, separate) = match component.cast::<IEditController>() {
            Some(c) => (Some(c), false),
            None => {
                let mut ccid: TUID = [0; 16];
                let c = if ok(unsafe { component.getControllerClassId(&mut ccid) }) {
                    unsafe { ComPtr::from_raw(create(&ccid, &IEditController::IID) as *mut IEditController) }
                } else {
                    None
                };
                let c = c.filter(|c| ok(unsafe { c.initialize(host_ptr) }));
                (c, true)
            }
        };
        let handler = ComWrapper::new(Handler::default());
        if let Some(c) = &controller {
            if let Some(h) = handler.as_com_ref::<IComponentHandler>() {
                unsafe { c.setComponentHandler(h.as_ptr()) };
            }
            if separate
                && let (Some(a), Some(b)) = (component.cast::<IConnectionPoint>(), c.cast::<IConnectionPoint>())
            {
                unsafe {
                    a.connect(b.as_ptr());
                    b.connect(a.as_ptr());
                }
            }
            // the controller learns the component's state
            let s = stream(Vec::new());
            if ok(unsafe { component.getState(stream_ptr(&s)) }) {
                s.pos.set(0);
                unsafe { c.setComponentState(stream_ptr(&s)) };
            }
        }

        // stereo in, stereo out on the main buses
        let n_in = unsafe { component.getBusCount(kAudio as i32, kInput as i32) };
        let n_out = unsafe { component.getBusCount(kAudio as i32, kOutput as i32) };
        if n_in < 1 || n_out < 1 {
            return Err("it takes no audio input (an instrument?)".into());
        }
        let arrangement = |dir: i32, n: i32| -> Vec<SpeakerArrangement> {
            (0..n)
                .map(|i| {
                    let mut a: SpeakerArrangement = 0;
                    unsafe { processor.getBusArrangement(dir, i, &mut a) };
                    if i == 0 { SpeakerArr::kStereo } else { a }
                })
                .collect()
        };
        let (mut ins, mut outs) = (arrangement(kInput as i32, n_in), arrangement(kOutput as i32, n_out));
        unsafe { processor.setBusArrangements(ins.as_mut_ptr(), n_in, outs.as_mut_ptr(), n_out) };
        let channels_of = |dir: i32| {
            let mut b: BusInfo = unsafe { std::mem::zeroed() };
            unsafe { component.getBusInfo(kAudio as i32, dir, 0, &mut b) };
            b.channelCount.clamp(1, 2)
        };
        let channels = (channels_of(kInput as i32), channels_of(kOutput as i32));
        for (dir, n) in [(kInput as i32, n_in), (kOutput as i32, n_out)] {
            for i in 0..n {
                unsafe { component.activateBus(kAudio as i32, dir, i, (i == 0) as TBool) };
            }
        }
        let mut setup = ProcessSetup {
            processMode: kRealtime as i32,
            symbolicSampleSize: kSample32 as i32,
            maxSamplesPerBlock: MAX_FRAMES as i32,
            sampleRate: rate,
        };
        if !ok(unsafe { processor.canProcessSampleSize(kSample32 as i32) }) {
            return Err("it doesn't take 32-bit audio".into());
        }
        unsafe {
            processor.setupProcessing(&mut setup);
            component.setActive(1);
            processor.setProcessing(1);
        }
        let latency = unsafe { processor.getLatencySamples() } as usize;

        let mut context: ProcessContext = unsafe { std::mem::zeroed() };
        context.sampleRate = rate;
        context.tempo = 120.0;
        context.timeSigNumerator = 4;
        context.timeSigDenominator = 4;
        context.state = kTempoValid | kTimeSigValid | kContTimeValid;
        let changes = ComWrapper::new(Changes {
            queues: (0..MAX_CHANGES).map(|_| ComWrapper::new(Queue::default())).collect(),
            count: Cell::new(0),
        });
        Ok(Arc::new(Self {
            info: info.clone(),
            component,
            processor,
            controller,
            separate,
            handler,
            _host: host,
            changes,
            channels,
            busy: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            latency: AtomicUsize::new(latency),
            io: std::cell::UnsafeCell::new(Io {
                input: [vec![0.0; MAX_FRAMES], vec![0.0; MAX_FRAMES]],
                in_ptrs: [std::ptr::null_mut(); 2],
                out_ptrs: [std::ptr::null_mut(); 2],
                context,
            }),
        }))
    }

    /// Re-reads its delay if it said it changed (main thread).
    pub fn refresh_latency(&self) {
        if self.handler.latency_changed.swap(false, Ordering::Relaxed) {
            self.latency.store(unsafe { self.processor.getLatencySamples() } as usize, Ordering::Relaxed);
        }
    }

    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    /// Its state: the component's and the controller's, base64. Main thread.
    pub fn state(&self) -> Option<String> {
        let comp = stream(Vec::new());
        if !ok(unsafe { self.component.getState(stream_ptr(&comp)) }) {
            return None;
        }
        let ctrl = stream(Vec::new());
        if let Some(c) = &self.controller {
            unsafe { c.getState(stream_ptr(&ctrl)) };
        }
        let (a, b) = (comp.data.borrow(), ctrl.data.borrow());
        let mut out = Vec::with_capacity(8 + a.len() + b.len());
        out.extend_from_slice(b"LAV3");
        out.extend_from_slice(&(a.len() as u32).to_le_bytes());
        out.extend_from_slice(&a);
        out.extend_from_slice(&b);
        Some(base64::engine::general_purpose::STANDARD.encode(out))
    }

    /// Restores a state from [`Self::state`]. Main thread.
    pub fn set_state(&self, state: &str) -> bool {
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(state) else { return false };
        if bytes.len() < 8 || &bytes[..4] != b"LAV3" {
            return false;
        }
        let n = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
        let Some(comp) = bytes.get(8..8 + n) else { return false };
        let ctrl = &bytes[8 + n..];
        let s = stream(comp.to_vec());
        let done = ok(unsafe { self.component.setState(stream_ptr(&s)) });
        if let Some(c) = &self.controller {
            s.pos.set(0);
            unsafe { c.setComponentState(stream_ptr(&s)) };
            if !ctrl.is_empty() {
                let s = stream(ctrl.to_vec());
                unsafe { c.setState(stream_ptr(&s)) };
            }
        }
        done
    }

    /// Its editor view, unattached, and its size. Main thread.
    pub fn create_view(&self) -> Option<(ComPtr<IPlugView>, (i32, i32))> {
        let c = self.controller.as_ref()?;
        let view = unsafe { ComPtr::from_raw(c.createView(c"editor".as_ptr())) }?;
        if !ok(unsafe { view.isPlatformTypeSupported(kPlatformTypeNSView) }) {
            return None;
        }
        let mut r = ViewRect { left: 0, top: 0, right: 0, bottom: 0 };
        unsafe { view.getSize(&mut r) };
        Some((view, (r.right - r.left, r.bottom - r.top)))
    }

    fn render(&self, l: &mut [f32], r: &mut [f32]) -> bool {
        let io = unsafe { &mut *self.io.get() };
        let n = l.len();
        // the editor's edits reach the processor at the start of the block
        let mut count = 0;
        if let Some(mut pending) = self.handler.pending.try_lock() {
            for (id, value) in pending.iter().take(MAX_CHANGES) {
                let q = &self.changes.queues[count];
                q.id.set(*id);
                q.value.set(*value);
                count += 1;
            }
            pending.clear();
        }
        self.changes.count.set(count);
        if self.channels.0 == 1 {
            for i in 0..n {
                io.input[0][i] = 0.5 * (l[i] + r[i]);
            }
        } else {
            io.input[0][..n].copy_from_slice(l);
            io.input[1][..n].copy_from_slice(r);
        }
        io.in_ptrs = [io.input[0].as_mut_ptr(), io.input[1].as_mut_ptr()];
        io.out_ptrs = [l.as_mut_ptr(), r.as_mut_ptr()];
        let mut inputs = AudioBusBuffers {
            numChannels: self.channels.0,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 { channelBuffers32: io.in_ptrs.as_mut_ptr() },
        };
        let mut outputs = AudioBusBuffers {
            numChannels: self.channels.1,
            silenceFlags: 0,
            __field0: AudioBusBuffers__type0 { channelBuffers32: io.out_ptrs.as_mut_ptr() },
        };
        let mut data = ProcessData {
            processMode: kRealtime as i32,
            symbolicSampleSize: kSample32 as i32,
            numSamples: n as i32,
            numInputs: 1,
            numOutputs: 1,
            inputs: &mut inputs,
            outputs: &mut outputs,
            inputParameterChanges: self
                .changes
                .as_com_ref::<IParameterChanges>()
                .map_or(std::ptr::null_mut(), |c| c.as_ptr()),
            outputParameterChanges: std::ptr::null_mut(),
            inputEvents: std::ptr::null_mut(),
            outputEvents: std::ptr::null_mut(),
            processContext: &mut io.context,
        };
        let res = unsafe { self.processor.process(&mut data) };
        io.context.projectTimeSamples += n as i64;
        io.context.continousTimeSamples += n as i64;
        self.changes.count.set(0);
        if !ok(res) {
            if self.channels.0 == 2 {
                l.copy_from_slice(&io.input[0][..n]);
                r.copy_from_slice(&io.input[1][..n]);
            }
            return false;
        }
        if self.channels.1 == 1 {
            r.copy_from_slice(l);
        }
        true
    }
}

impl Plugin for Vst3Plugin {
    fn process(&self, l: &mut [f32], r: &mut [f32]) -> bool {
        if self.busy.swap(true, Ordering::Acquire) {
            return false;
        }
        let n = l.len().min(r.len());
        let mut ok = true;
        let mut at = 0;
        while at < n {
            let end = (at + MAX_FRAMES).min(n);
            ok &= self.render(&mut l[at..end], &mut r[at..end]);
            at = end;
        }
        if !ok {
            self.failed.store(true, Ordering::Relaxed);
        }
        self.busy.store(false, Ordering::Release);
        ok
    }

    fn latency(&self) -> usize {
        self.latency.load(Ordering::Relaxed)
    }
}

/// The parts torn down on the main thread.
struct Teardown {
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    controller: Option<ComPtr<IEditController>>,
    separate: bool,
}

unsafe impl Send for Teardown {}

impl Drop for Vst3Plugin {
    fn drop(&mut self) {
        // ComPtrs can't be moved out of `self`: clone the references for the main thread
        let t = Teardown {
            component: self.component.clone(),
            processor: self.processor.clone(),
            controller: self.controller.clone(),
            separate: self.separate,
        };
        super::on_main(Box::new(move || unsafe {
            t.processor.setProcessing(0);
            t.component.setActive(0);
            if let Some(c) = &t.controller {
                c.setComponentHandler(std::ptr::null_mut());
                if t.separate {
                    if let (Some(a), Some(b)) = (t.component.cast::<IConnectionPoint>(), c.cast::<IConnectionPoint>()) {
                        a.disconnect(b.as_ptr());
                        b.disconnect(a.as_ptr());
                    }
                    c.terminate();
                }
            }
            t.component.terminate();
        }));
    }
}

// ------------------------------------------------------------------ an editor

/// The host side of an editor's window: the plugin asks it to resize.
struct Frame {
    resize: Box<dyn Fn(i32, i32)>,
}

impl Class for Frame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for Frame {
    unsafe fn resizeView(&self, view: *mut IPlugView, size: *mut ViewRect) -> tresult {
        if size.is_null() {
            return kInvalidArgument;
        }
        let r = unsafe { *size };
        (self.resize)(r.right - r.left, r.bottom - r.top);
        if let Some(v) = unsafe { vst3::ComRef::from_raw(view) } {
            unsafe { v.onSize(size) };
        }
        kResultOk
    }
}

/// A plugin's editor attached to a window's view. Dropping it detaches it. Main thread.
pub struct Vst3View {
    view: ComPtr<IPlugView>,
    _frame: ComWrapper<Frame>,
}

impl Vst3View {
    /// Attaches `view` to the NSView `parent`; `resize` sets the window's content size when the
    /// plugin asks.
    ///
    /// # Safety
    /// `parent` is a live NSView that outlives the returned view.
    pub unsafe fn attach(view: ComPtr<IPlugView>, parent: *mut c_void, resize: impl Fn(i32, i32) + 'static) -> Option<Self> {
        let frame = ComWrapper::new(Frame { resize: Box::new(resize) });
        unsafe {
            if let Some(f) = frame.as_com_ref::<IPlugFrame>() {
                view.setFrame(f.as_ptr());
            }
            if !ok(view.attached(parent, kPlatformTypeNSView)) {
                view.setFrame(std::ptr::null_mut());
                return None;
            }
        }
        Some(Self { view, _frame: frame })
    }

    /// Whether the window may be resized (the plugin follows).
    pub fn can_resize(&self) -> bool {
        ok(unsafe { self.view.canResize() })
    }

    /// The window was resized to `w`×`h`.
    pub fn resized(&self, w: i32, h: i32) {
        let mut r = ViewRect { left: 0, top: 0, right: w, bottom: h };
        unsafe {
            self.view.checkSizeConstraint(&mut r);
            self.view.onSize(&mut r);
        }
    }
}

impl Drop for Vst3View {
    fn drop(&mut self) {
        unsafe {
            self.view.removed();
            self.view.setFrame(std::ptr::null_mut());
        }
    }
}
