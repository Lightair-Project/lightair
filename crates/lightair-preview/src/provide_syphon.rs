//! Syphon inputs (`syphon-input`) received for the compositor, with `LIGHTAIR_ENGINE=own`.
//!
//! - **One client per source while shown**, made and let go on a thread of this module's own
//!   (connecting to a server talks to another process), a second after no compositor shows the
//!   source. A server that isn't there yet, or went away, is looked for again every second.
//! - **Pictures.** Syphon hands each new frame as the server's IOSurface. Servers draw every
//!   frame into the same surface, and its colours aren't multiplied by alpha, so it isn't handed
//!   on as it is: each frame is copied (in Syphon's frame callback) into a buffer the compositor
//!   isn't holding, with colours multiplied by alpha, or alpha set to 1 when the source doesn't
//!   allow transparency (as the libobs source draws it).
//! - **What stays libobs's:** a source with its crop on, and every Syphon source when Syphon's
//!   framework isn't loaded. The libobs source's own client keeps running alongside: a Syphon
//!   client only reads the server's surface, so two cost no more than one, and libobs's picture
//!   of the source (thumbnails, Studio mode, effects the compositor doesn't draw) stays right.
//!
//! Syphon's framework comes with the module that gives libobs its Syphon source, so its classes
//! are found at run time; when that module isn't loaded, the framework is loaded from the app's
//! Frameworks folder.

use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use lightair_model::{Id, Source};
use objc2::rc::{Retained, autoreleasepool};
use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
use objc2::{msg_send, sel};
use objc2_core_foundation::{CFRetained, CFString};
use objc2_io_surface::IOSurfaceRef;
use parking_lot::Mutex;

use crate::provide_ndi::{Alpha, Latest, Pool, copy_bgra};
use crate::providers::{Frame, Provider};

/// The libobs source kind.
const KIND: &str = "syphon-input";
/// How long a client keeps running once no compositor shows its source.
const LINGER: Duration = Duration::from_secs(1);
/// How often the clients are looked after.
const PERIOD: Duration = Duration::from_millis(50);
/// A server that wasn't found, or went away, is looked for again after this.
const RETRY: Duration = Duration::from_secs(1);
/// `'BGRA'`, the format Syphon's surfaces have.
const BGRA: u32 = u32::from_be_bytes(*b"BGRA");
/// `kIOSurfaceLockReadOnly`.
const LOCK_READ_ONLY: u32 = 1;

#[link(name = "IOSurface", kind = "framework")]
unsafe extern "C" {
    fn IOSurfaceLock(surface: &IOSurfaceRef, options: u32, seed: *mut u32) -> i32;
    fn IOSurfaceUnlock(surface: &IOSurfaceRef, options: u32, seed: *mut u32) -> i32;
}

/// Syphon's classes are there (loading its framework from the app bundle if they aren't yet).
fn framework() -> bool {
    static FOUND: OnceLock<bool> = OnceLock::new();
    *FOUND.get_or_init(|| {
        if AnyClass::get(c"SyphonClientBase").is_some() {
            return true;
        }
        // Lightair.app/Contents/MacOS/<exe> → Lightair.app/Contents/Frameworks
        let path = std::env::current_exe()
            .ok()
            .and_then(|exe| Some(exe.parent()?.parent()?.join("Frameworks/Syphon.framework/Syphon")));
        if let Some(path) = path.filter(|p| p.exists()) {
            // SAFETY: Syphon's framework only registers its classes on load.
            match unsafe { libloading::Library::new(&path) } {
                // kept loaded for the rest of the run: its classes stay registered
                Ok(lib) => std::mem::forget(lib),
                Err(e) => tracing::warn!(%e, "Syphon provider: couldn't load Syphon's framework"),
            }
        }
        let found = AnyClass::get(c"SyphonClientBase").is_some() && AnyClass::get(c"SyphonServerDirectory").is_some();
        if !found {
            tracing::info!("Syphon provider: no Syphon framework; Syphon sources stay libobs's");
        }
        found
    })
}

/// Which server a source shows, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Wanted {
    uuid: String,
    app: String,
    name: String,
    /// Alpha set to 1 (the source doesn't allow transparency).
    opaque: bool,
}

impl Wanted {
    fn of(source: &Source) -> Wanted {
        let s = |k: &str| source.settings.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_owned();
        let flag = |k: &str| source.settings.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
        Wanted { uuid: s("uuid"), app: s("app_name"), name: s("name"), opaque: !flag("allow_transparency") }
    }
}

/// Receives the Syphon inputs the compositors show.
#[derive(Default)]
pub(crate) struct Syphon {
    state: Arc<Mutex<State>>,
}

#[derive(Default)]
struct State {
    /// What each canvas's compositor shows.
    wanted: HashMap<Id, HashMap<Id, Wanted>>,
    clients: HashMap<Id, Entry>,
    /// The thread looking after the clients is running.
    looking_after: bool,
}

struct Entry {
    wanted: Wanted,
    /// When the client stops unless a compositor shows its source again (`None`: shown).
    stop_at: Option<Instant>,
    /// When to look for the server again while there's no client.
    next_try: Instant,
    slot: Arc<Slot>,
    client: Option<Client>,
}

/// Where a client's frames go: the newest picture, and the buffers it's copied into.
struct Slot {
    alpha: Alpha,
    latest: Mutex<Option<Latest>>,
    pool: Mutex<Pool>,
    serial: AtomicU64,
}

impl Provider for Syphon {
    fn handles(&self, source: &Source) -> bool {
        source.kind == KIND
            && !source.settings.get("crop").and_then(|v| v.as_bool()).unwrap_or(false)
            && ["uuid", "name", "app_name"]
                .iter()
                .any(|k| source.settings.get(*k).and_then(|v| v.as_str()).is_some_and(|s| !s.is_empty()))
            && framework()
    }

    fn want(&self, canvas: Id, shown: &[&Source]) {
        let mut st = self.state.lock();
        if shown.is_empty() {
            st.wanted.remove(&canvas);
        } else {
            st.wanted.insert(canvas, shown.iter().map(|s| (s.id, Wanted::of(s))).collect());
        }
        if !st.looking_after && !st.wanted.is_empty() {
            let state = self.state.clone();
            let spawned =
                std::thread::Builder::new().name("lightair-syphon-provider".into()).spawn(move || look_after(state));
            st.looking_after = spawned.is_ok();
        }
    }

    fn frame(&self, source: Id) -> Option<Frame> {
        let slot = self.state.lock().clients.get(&source)?.slot.clone();
        slot.latest.lock().as_ref().map(Latest::frame)
    }
}

/// The thread that makes, checks and lets go of the clients, until none is wanted or running.
fn look_after(state: Arc<Mutex<State>>) {
    loop {
        let now = Instant::now();
        let mut stopping: Vec<Client> = Vec::new();
        let mut starting: Vec<(Id, Wanted, Arc<Slot>)> = Vec::new();
        let done = {
            let mut st = state.lock();
            let all: HashMap<Id, Wanted> =
                st.wanted.values().flat_map(|m| m.iter().map(|(id, w)| (*id, w.clone()))).collect();
            st.clients.retain(|id, entry| {
                let keep = match all.get(id) {
                    Some(w) if *w == entry.wanted => {
                        entry.stop_at = None;
                        true
                    }
                    // another server or alpha now: made again below
                    Some(_) => false,
                    None => now < *entry.stop_at.get_or_insert(now + LINGER),
                };
                if !keep {
                    stopping.extend(entry.client.take());
                }
                keep
            });
            for (id, wanted) in all {
                let entry = st.clients.entry(id).or_insert_with(|| Entry {
                    slot: Arc::new(Slot {
                        alpha: if wanted.opaque { Alpha::Opaque } else { Alpha::Straight },
                        latest: Mutex::new(None),
                        pool: Mutex::new(Pool::default()),
                        serial: AtomicU64::new(0),
                    }),
                    wanted: wanted.clone(),
                    stop_at: None,
                    next_try: now,
                    client: None,
                });
                // the server went away: look for it again in a moment (its last picture stays)
                if entry.client.as_ref().is_some_and(|c| !c.valid()) {
                    stopping.extend(entry.client.take());
                    entry.next_try = now + RETRY;
                }
                if entry.client.is_none() && now >= entry.next_try {
                    entry.next_try = now + RETRY;
                    starting.push((id, wanted, entry.slot.clone()));
                }
            }
            // nothing to look after: this thread ends (`want` starts another when needed)
            let done = st.clients.is_empty() && st.wanted.is_empty();
            if done {
                st.looking_after = false;
            }
            done
        };
        for client in stopping {
            client.stop();
        }
        for (id, wanted, slot) in starting {
            let Some(client) = Client::open(&wanted, slot) else { continue };
            let mut st = state.lock();
            match st.clients.get_mut(&id) {
                Some(entry) if entry.wanted == wanted && entry.client.is_none() => entry.client = Some(client),
                _ => {
                    drop(st);
                    client.stop();
                }
            }
        }
        if done {
            return;
        }
        std::thread::sleep(PERIOD);
    }
}

// ---------------------------------------------------------------- Syphon's client

/// A Syphon client (`SyphonClientBase`) receiving one server's frames.
struct Client(Retained<AnyObject>);

// SAFETY: Syphon's clients may be used and released from any thread.
unsafe impl Send for Client {}

/// `NSString` for `s` (toll-free bridged with `CFString`).
fn ns_string(s: &str) -> CFRetained<CFString> {
    CFString::from_str(s)
}

/// The string `dict[key]`, when it's one.
///
/// # Safety
///
/// `dict` must be an `NSDictionary`.
unsafe fn string_at(dict: *mut AnyObject, key: &CFString) -> String {
    let key = key as *const CFString as *const AnyObject;
    // SAFETY: `dict` is a dictionary; the key is a string.
    let value: *mut AnyObject = unsafe { msg_send![dict, objectForKey: key] };
    let Some(value) = NonNull::new(value) else { return String::new() };
    // SAFETY: Syphon's descriptions keep strings under these keys (NSString is a CFString).
    let is_string: Bool = unsafe { msg_send![value.as_ptr(), isKindOfClass: AnyClass::get(c"NSString")] };
    if !is_string.as_bool() {
        return String::new();
    }
    unsafe { &*(value.as_ptr() as *const CFString) }.to_string()
}

impl Client {
    /// A client of the server `wanted` names (by its UUID, else by its app and name), sending
    /// each new frame into `slot`. `None` while there's no such server.
    fn open(wanted: &Wanted, slot: Arc<Slot>) -> Option<Client> {
        let directory = AnyClass::get(c"SyphonServerDirectory")?;
        let class = AnyClass::get(c"SyphonClientBase")?;
        let (uuid_key, name_key, app_key) = (
            ns_string("SyphonServerDescriptionUUIDKey"),
            ns_string("SyphonServerDescriptionNameKey"),
            ns_string("SyphonServerDescriptionAppNameKey"),
        );
        autoreleasepool(|_| unsafe {
            let directory: *mut AnyObject = msg_send![directory, sharedDirectory];
            let servers: *mut AnyObject = msg_send![directory, servers];
            if servers.is_null() {
                return None;
            }
            let count: usize = msg_send![servers, count];
            let descriptions: Vec<*mut AnyObject> = (0..count).map(|i| msg_send![servers, objectAtIndex: i]).collect();
            let by_uuid = || {
                descriptions
                    .iter()
                    .copied()
                    .find(|&d| !wanted.uuid.is_empty() && string_at(d, &uuid_key) == wanted.uuid)
            };
            // a server started again has another UUID: then its app and name find it
            let by_name = || {
                descriptions.iter().copied().find(|&d| {
                    (!wanted.app.is_empty() || !wanted.name.is_empty())
                        && (wanted.app.is_empty() || string_at(d, &app_key) == wanted.app)
                        && (wanted.name.is_empty() || string_at(d, &name_key) == wanted.name)
                })
            };
            let description = by_uuid().or_else(by_name)?;
            let handler = block2::RcBlock::new(move |client: *mut AnyObject| {
                if !client.is_null() {
                    autoreleasepool(|_| slot.receive(client));
                }
            });
            let client: *mut AnyObject = msg_send![class, alloc];
            let client: *mut AnyObject = msg_send![
                client,
                initWithServerDescription: description,
                options: std::ptr::null_mut::<AnyObject>(),
                newFrameHandler: &*handler
            ];
            // SAFETY: `init…` gave a client we own one reference to (or nil).
            Retained::from_raw(client).map(Client)
        })
    }

    /// The server is still there.
    fn valid(&self) -> bool {
        // SAFETY: a Syphon client answers `isValid`.
        let valid: Bool = unsafe { msg_send![&*self.0, isValid] };
        valid.as_bool()
    }

    /// Stops receiving (the client is released after).
    fn stop(self) {
        // SAFETY: a Syphon client answers `stop`.
        autoreleasepool(|_| unsafe {
            let _: () = msg_send![&*self.0, stop];
        });
    }
}

impl Slot {
    /// Syphon's callback for a new frame of `client`: copies the server's surface into a buffer
    /// of its own as the newest picture.
    fn receive(&self, client: *mut AnyObject) {
        // `newSurface` gives the server's surface retained (or null); it's called through
        // `objc_msgSend` directly because it returns a C pointer, not an object
        type NewSurface = unsafe extern "C-unwind" fn(*mut AnyObject, Sel) -> *mut c_void;
        // SAFETY: `objc_msgSend` called with the method's own signature.
        let surface = unsafe {
            let send: NewSurface = std::mem::transmute(objc2::ffi::objc_msgSend as unsafe extern "C-unwind" fn());
            send(client, sel!(newSurface))
        };
        let Some(surface) = NonNull::new(surface.cast::<IOSurfaceRef>()) else { return };
        // SAFETY: `newSurface` returned a reference we own.
        let surface: CFRetained<IOSurfaceRef> = unsafe { CFRetained::from_raw(surface) };
        let (w, h) = (surface.width() as u32, surface.height() as u32);
        if w == 0 || h == 0 || surface.bytes_per_element() != 4 || ![BGRA, 0].contains(&surface.pixel_format()) {
            return;
        }
        let Some(out) = self.pool.lock().take(w, h) else { return };
        // SAFETY: the surface is locked for reading while its rows are copied.
        let copied = unsafe {
            if IOSurfaceLock(&surface, LOCK_READ_ONLY, std::ptr::null_mut()) != 0 {
                return;
            }
            let ok = copy_bgra(
                surface.base_address().as_ptr() as *const u8,
                surface.bytes_per_row(),
                w,
                h,
                &out,
                self.alpha,
            );
            IOSurfaceUnlock(&surface, LOCK_READ_ONLY, std::ptr::null_mut());
            ok
        };
        if copied {
            let serial = self.serial.fetch_add(1, Ordering::Relaxed) + 1;
            *self.latest.lock() = Some(Latest { buffer: out, serial });
        }
    }
}
