//! Desktop and app audio through Core Audio process taps (macOS 14.2+): `lightair_system_audio`.
//!
//! OBS's audio capture (`sck_audio_capture`) gets the sound through a ScreenCaptureKit stream,
//! which has the window server compose the display for it all along: one desktop audio source
//! cost the window server about a tenth of a core. A process tap is Core Audio's own: the sound
//! the Mac plays, mixed (everything but Lightair, so monitoring doesn't come back), or one app's
//! (by bundle ID, followed as it quits and opens again), with no picture involved.
//!
//! Settings: `type` 0 everything (desktop audio), 1 one app; `application` its bundle ID.
//!
//! The tap is read through a private aggregate device (the default output as its clock) whose
//! I/O callback hands the samples to libobs with their host-time timestamps. macOS asks the user
//! the first time ("System Audio Recording"): that, and making the tap, happen on a thread of
//! the source's own, never on the engine's.

use std::ffi::{CStr, c_char, c_void};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use objc2::msg_send;
use objc2::rc::autoreleasepool;
use objc2::runtime::{AnyClass, AnyObject};
use parking_lot::{Condvar, Mutex};

use crate::sys;

/// The libobs source type.
pub const ID: &CStr = c"lightair_system_audio";

// ------------------------------------------------------------------ Core Audio

#[repr(C)]
#[derive(Clone, Copy)]
struct Address {
    selector: u32,
    scope: u32,
    element: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Asbd {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioBuffer {
    channels: u32,
    bytes: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    count: u32,
    buffers: [AudioBuffer; 1],
}

#[repr(C)]
struct AudioTimeStamp {
    sample_time: f64,
    host_time: u64,
    rate_scalar: f64,
    word_clock_time: u64,
    smpte: [u8; 24],
    flags: u32,
    reserved: u32,
}

type IoProc = unsafe extern "C" fn(
    device: u32,
    now: *const AudioTimeStamp,
    input: *const AudioBufferList,
    input_time: *const AudioTimeStamp,
    output: *mut AudioBufferList,
    output_time: *const AudioTimeStamp,
    client: *mut c_void,
) -> i32;

#[link(name = "CoreAudio", kind = "framework")]
unsafe extern "C" {
    fn AudioObjectGetPropertyDataSize(object: u32, address: *const Address, qualifier_size: u32, qualifier: *const c_void, size: *mut u32) -> i32;
    fn AudioObjectGetPropertyData(
        object: u32,
        address: *const Address,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
        data: *mut c_void,
    ) -> i32;
    fn AudioHardwareCreateProcessTap(description: *mut AnyObject, tap: *mut u32) -> i32;
    fn AudioHardwareDestroyProcessTap(tap: u32) -> i32;
    fn AudioHardwareCreateAggregateDevice(description: *const c_void, device: *mut u32) -> i32;
    fn AudioHardwareDestroyAggregateDevice(device: u32) -> i32;
    fn AudioDeviceCreateIOProcID(device: u32, proc_: IoProc, client: *mut c_void, id: *mut *mut c_void) -> i32;
    fn AudioDeviceDestroyIOProcID(device: u32, id: *mut c_void) -> i32;
    fn AudioDeviceStart(device: u32, id: *mut c_void) -> i32;
    fn AudioDeviceStop(device: u32, id: *mut c_void) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
}

unsafe extern "C" {
    fn mach_timebase_info(info: *mut [u32; 2]) -> i32;
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*s)
}

const SYSTEM: u32 = 1;
const GLOBAL: u32 = fourcc(b"glob");
const TRANSLATE_PID: u32 = fourcc(b"id2p");
const PROCESS_LIST: u32 = fourcc(b"prs#");
const PROCESS_BUNDLE_ID: u32 = fourcc(b"pbid");
const TAP_UID: u32 = fourcc(b"tuid");
const TAP_FORMAT: u32 = fourcc(b"tfmt");
const DEFAULT_OUTPUT: u32 = fourcc(b"dOut");
const DEVICE_UID: u32 = fourcc(b"uid ");
const NON_INTERLEAVED: u32 = 1 << 5;

fn address(selector: u32) -> Address {
    Address { selector, scope: GLOBAL, element: 0 }
}

/// A property that's one value of type T.
unsafe fn get<T: Default>(object: u32, selector: u32, qualifier: Option<&u32>) -> Option<T> {
    let mut value = T::default();
    let mut size = std::mem::size_of::<T>() as u32;
    let (qs, q) = qualifier.map_or((0, std::ptr::null()), |q| (4, q as *const u32 as *const c_void));
    let rc = unsafe { AudioObjectGetPropertyData(object, &address(selector), qs, q, &mut size, &mut value as *mut T as *mut c_void) };
    (rc == 0).then_some(value)
}

/// A property that's a CFString, as text.
unsafe fn get_string(object: u32, selector: u32) -> Option<String> {
    let s: usize = unsafe { get(object, selector, None)? };
    if s == 0 {
        return None;
    }
    let text = autoreleasepool(|_| unsafe {
        let utf8: *const c_char = msg_send![s as *mut AnyObject, UTF8String];
        (!utf8.is_null()).then(|| CStr::from_ptr(utf8).to_string_lossy().into_owned())
    });
    unsafe { CFRelease(s as *const c_void) };
    text
}

/// Every audio process object Core Audio knows.
unsafe fn processes() -> Vec<u32> {
    let mut size = 0u32;
    if unsafe { AudioObjectGetPropertyDataSize(SYSTEM, &address(PROCESS_LIST), 0, std::ptr::null(), &mut size) } != 0 {
        return Vec::new();
    }
    let mut list = vec![0u32; size as usize / 4];
    let rc = unsafe {
        AudioObjectGetPropertyData(SYSTEM, &address(PROCESS_LIST), 0, std::ptr::null(), &mut size, list.as_mut_ptr() as *mut c_void)
    };
    if rc != 0 {
        return Vec::new();
    }
    list.truncate(size as usize / 4);
    list
}

/// The process objects of the app with this bundle ID (an app's helpers included).
unsafe fn app_processes(bundle: &str) -> Vec<u32> {
    unsafe { processes() }
        .into_iter()
        .filter(|&p| unsafe { get_string(p, PROCESS_BUNDLE_ID) }.is_some_and(|b| b == bundle || b.starts_with(&format!("{bundle}."))))
        .collect()
}

fn class(name: &CStr) -> Option<&'static AnyClass> {
    AnyClass::get(name)
}

unsafe fn ns_string(s: &str) -> *mut AnyObject {
    let c = std::ffi::CString::new(s).unwrap_or_default();
    unsafe { msg_send![class(c"NSString").unwrap(), stringWithUTF8String: c.as_ptr()] }
}

unsafe fn ns_number(v: i64) -> *mut AnyObject {
    unsafe { msg_send![class(c"NSNumber").unwrap(), numberWithLongLong: v] }
}

unsafe fn ns_array(items: &[*mut AnyObject]) -> *mut AnyObject {
    unsafe {
        let a: *mut AnyObject = msg_send![class(c"NSMutableArray").unwrap(), array];
        for &i in items {
            let _: () = msg_send![a, addObject: i];
        }
        a
    }
}

unsafe fn ns_dictionary(pairs: &[(&str, *mut AnyObject)]) -> *mut AnyObject {
    unsafe {
        let d: *mut AnyObject = msg_send![class(c"NSMutableDictionary").unwrap(), dictionary];
        for &(k, v) in pairs {
            let _: () = msg_send![d, setObject: v, forKey: ns_string(k)];
        }
        d
    }
}

/// A running tap: the tap, its aggregate device and I/O callback.
struct Tap {
    tap: u32,
    device: u32,
    proc_id: *mut c_void,
    /// What the callback reads (kept alive while the device runs).
    _reader: Box<Reader>,
}
// SAFETY: Core Audio objects are ids, usable from any thread.
unsafe impl Send for Tap {}

struct Reader {
    source: usize,
    format: Asbd,
    /// Host time to nanoseconds.
    timebase: (u64, u64),
}

impl Drop for Tap {
    fn drop(&mut self) {
        unsafe {
            AudioDeviceStop(self.device, self.proc_id);
            AudioDeviceDestroyIOProcID(self.device, self.proc_id);
            AudioHardwareDestroyAggregateDevice(self.device);
            AudioHardwareDestroyProcessTap(self.tap);
        }
    }
}

/// The tap's samples to libobs (Core Audio's I/O thread).
unsafe extern "C" fn read(
    _device: u32,
    _now: *const AudioTimeStamp,
    input: *const AudioBufferList,
    input_time: *const AudioTimeStamp,
    _output: *mut AudioBufferList,
    _output_time: *const AudioTimeStamp,
    client: *mut c_void,
) -> i32 {
    let r = unsafe { &*(client as *const Reader) };
    let Some(list) = (unsafe { input.as_ref() }) else { return 0 };
    if list.count == 0 {
        return 0;
    }
    let buffers = unsafe { std::slice::from_raw_parts(list.buffers.as_ptr(), list.count as usize) };
    let channels = r.format.channels_per_frame.max(1);
    let planar = r.format.format_flags & NON_INTERLEAVED != 0;
    let frames = if planar { buffers[0].bytes / 4 } else { buffers[0].bytes / 4 / channels };
    if frames == 0 {
        return 0;
    }
    let host = unsafe { input_time.as_ref() }.map_or(0, |t| t.host_time);
    let timestamp = if host > 0 { host as u128 * r.timebase.0 as u128 / r.timebase.1 as u128 } else { (unsafe { sys::os_gettime_ns() }) as u128 };
    let mut audio: sys::obs_source_audio = unsafe { std::mem::zeroed() };
    if planar {
        for (slot, b) in audio.data.iter_mut().zip(buffers) {
            *slot = b.data as *const u8;
        }
        audio.format = sys::AUDIO_FORMAT_FLOAT_PLANAR;
    } else {
        audio.data[0] = buffers[0].data as *const u8;
        audio.format = sys::AUDIO_FORMAT_FLOAT;
    }
    audio.frames = frames;
    audio.speakers = if channels == 1 { sys::SPEAKERS_MONO } else { sys::SPEAKERS_STEREO };
    audio.samples_per_sec = r.format.sample_rate as u32;
    audio.timestamp = timestamp as u64;
    unsafe { sys::obs_source_output_audio(r.source as *mut sys::obs_source_t, &audio) };
    0
}

/// Makes a tap of everything but Lightair (`app` none) or of an app's processes, and starts it.
fn start_tap(source: usize, processes: Option<&[u32]>) -> Result<Tap, String> {
    autoreleasepool(|_| unsafe {
        let class = class(c"CATapDescription").ok_or("process taps need macOS 14.2")?;
        let desc: *mut AnyObject = msg_send![class, alloc];
        let desc: *mut AnyObject = match processes {
            Some(list) => {
                let ids: Vec<*mut AnyObject> = list.iter().map(|&p| ns_number(p as i64)).collect();
                msg_send![desc, initStereoMixdownOfProcesses: ns_array(&ids)]
            }
            None => {
                let own = get::<u32>(SYSTEM, TRANSLATE_PID, Some(&(std::process::id())));
                let ids: Vec<*mut AnyObject> = own.filter(|&o| o != 0).map(|o| ns_number(o as i64)).into_iter().collect();
                msg_send![desc, initStereoGlobalTapButExcludeProcesses: ns_array(&ids)]
            }
        };
        if desc.is_null() {
            return Err("no tap description".into());
        }
        let _: () = msg_send![desc, setPrivate: true];
        let mut tap = 0u32;
        let rc = AudioHardwareCreateProcessTap(desc, &mut tap);
        let _: () = msg_send![desc, release];
        if rc != 0 {
            return Err(format!("AudioHardwareCreateProcessTap: {rc}"));
        }
        let fail = |what: &str, rc: i32, tap: u32| {
            AudioHardwareDestroyProcessTap(tap);
            Err(format!("{what}: {rc}"))
        };
        let Some(tap_uid) = get_string(tap, TAP_UID) else { return fail("the tap's UID", -1, tap) };
        let format: Asbd = get(tap, TAP_FORMAT, None).unwrap_or_default();
        let output = get::<u32>(SYSTEM, DEFAULT_OUTPUT, None).and_then(|d| get_string(d, DEVICE_UID));
        let uid = format!("app.lightair.tap.{}.{}", std::process::id(), tap);
        let mut pairs = vec![
            ("name", ns_string("Lightair system audio")),
            ("uid", ns_string(&uid)),
            ("private", ns_number(1)),
            ("stacked", ns_number(0)),
            ("tapautostart", ns_number(1)),
            ("taps", ns_array(&[ns_dictionary(&[("uid", ns_string(&tap_uid)), ("drift", ns_number(1))])])),
        ];
        if let Some(out) = &output {
            pairs.push(("master", ns_string(out)));
            pairs.push(("subdevices", ns_array(&[ns_dictionary(&[("uid", ns_string(out))])])));
        }
        let mut device = 0u32;
        let rc = AudioHardwareCreateAggregateDevice(ns_dictionary(&pairs) as *const c_void, &mut device);
        if rc != 0 {
            return fail("AudioHardwareCreateAggregateDevice", rc, tap);
        }
        let mut tb = [0u32; 2];
        mach_timebase_info(&mut tb);
        let reader = Box::new(Reader { source, format, timebase: (tb[0].max(1) as u64, tb[1].max(1) as u64) });
        let mut proc_id = std::ptr::null_mut();
        let rc = AudioDeviceCreateIOProcID(device, read, &*reader as *const Reader as *mut c_void, &mut proc_id);
        if rc != 0 {
            AudioHardwareDestroyAggregateDevice(device);
            return fail("AudioDeviceCreateIOProcID", rc, tap);
        }
        let running = Tap { tap, device, proc_id, _reader: reader };
        let rc = AudioDeviceStart(device, proc_id);
        if rc != 0 {
            drop(running);
            return Err(format!("AudioDeviceStart: {rc}"));
        }
        Ok(running)
    })
}

// ------------------------------------------------------------------ the libobs source

#[derive(Clone, PartialEq)]
struct Settings {
    app: Option<String>,
}

struct Shared {
    source: usize,
    settings: Mutex<Settings>,
    changed: Condvar,
    quit: AtomicBool,
}

struct SystemAudio {
    shared: Arc<Shared>,
    thread: Option<std::thread::JoinHandle<()>>,
}

fn settings_of(settings: *mut sys::obs_data_t) -> Settings {
    unsafe {
        let app = sys::obs_data_get_int(settings, c"type".as_ptr()) == 1;
        let bundle = sys::obs_data_get_string(settings, c"application".as_ptr());
        let bundle = if bundle.is_null() { String::new() } else { CStr::from_ptr(bundle).to_string_lossy().into_owned() };
        Settings { app: app.then_some(bundle) }
    }
}

/// Keeps a tap matching the settings running: makes it, makes it again when the app's
/// processes change (it quit, opened again) or the settings do.
fn run(shared: Arc<Shared>) {
    let mut tap: Option<Tap> = None;
    let mut tapped: Option<(Settings, Vec<u32>)> = None;
    let mut failed_for: Option<Settings> = None;
    loop {
        let settings = shared.settings.lock().clone();
        if shared.quit.load(Ordering::Acquire) {
            return;
        }
        let processes = match &settings.app {
            Some(bundle) if bundle.is_empty() => Some(Vec::new()),
            Some(bundle) => Some(unsafe { app_processes(bundle) }),
            None => None,
        };
        let want = (settings.clone(), processes.clone().unwrap_or_default());
        if tapped.as_ref() != Some(&want) && failed_for.as_ref() != Some(&settings) {
            tap = None;
            tapped = None;
            if processes.as_ref().is_none_or(|p| !p.is_empty()) {
                match start_tap(shared.source, processes.as_deref()) {
                    Ok(t) => {
                        tap = Some(t);
                        tapped = Some(want);
                        failed_for = None;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "system audio: no tap");
                        failed_for = Some(settings.clone());
                    }
                }
            }
        }
        // the app's processes are looked for again every couple of seconds; desktop audio's tap
        // just runs
        let mut s = shared.settings.lock();
        if *s == settings && !shared.quit.load(Ordering::Acquire) {
            shared.changed.wait_for(&mut s, Duration::from_secs(2));
        }
        drop(s);
        if shared.quit.load(Ordering::Acquire) {
            drop(tap.take());
            return;
        }
    }
}

unsafe extern "C" fn get_name(_: *mut c_void) -> *const c_char {
    c"System audio".as_ptr()
}

unsafe extern "C" fn create(settings: *mut sys::obs_data_t, source: *mut sys::obs_source_t) -> *mut c_void {
    let shared = Arc::new(Shared {
        source: source as usize,
        settings: Mutex::new(settings_of(settings)),
        changed: Condvar::new(),
        quit: AtomicBool::new(false),
    });
    let thread = {
        let shared = shared.clone();
        std::thread::Builder::new().name("lightair-system-audio".into()).spawn(move || run(shared)).ok()
    };
    Box::into_raw(Box::new(SystemAudio { shared, thread })) as *mut c_void
}

unsafe extern "C" fn destroy(data: *mut c_void) {
    let mut s = unsafe { Box::from_raw(data as *mut SystemAudio) };
    s.shared.quit.store(true, Ordering::Release);
    s.shared.changed.notify_all();
    if let Some(t) = s.thread.take() {
        let _ = t.join();
    }
}

unsafe extern "C" fn update(data: *mut c_void, settings: *mut sys::obs_data_t) {
    let s = unsafe { &*(data as *const SystemAudio) };
    *s.shared.settings.lock() = settings_of(settings);
    s.shared.changed.notify_all();
}

/// Registers the source with libobs (once; after the modules load).
pub fn register() {
    static DONE: std::sync::Once = std::sync::Once::new();
    DONE.call_once(|| unsafe {
        let mut info: sys::obs_source_info = std::mem::zeroed();
        info.id = ID.as_ptr();
        info.type_ = sys::OBS_SOURCE_TYPE_INPUT;
        info.output_flags = sys::OBS_SOURCE_AUDIO | sys::OBS_SOURCE_DO_NOT_DUPLICATE;
        info.get_name = Some(get_name);
        info.create = Some(create);
        info.destroy = Some(destroy);
        info.update = Some(update);
        info.icon_type = sys::OBS_ICON_TYPE_AUDIO_OUTPUT;
        sys::obs_register_source_s(&info, std::mem::size_of::<sys::obs_source_info>());
    });
}
