//! The macOS virtual camera's sink: pictures queued on Lightair's camera extension's sink stream
//! (CoreMediaIO), as the camera shows them.

use std::ffi::c_void;
use std::ptr;

use objc2_core_foundation::{CFRetained, CFString};
use objc2_core_video::CVPixelBuffer;

/// Frames a second the camera shows (its extension's rate).
pub const FPS: u32 = 60;

/// Lightair Virtual Camera's device (LightairCameraDeviceUUID in its extension's Info.plist).
const LIGHTAIR_CAMERA_UUID: &str = "2DF109AC-DE07-42DF-A0D7-098033FFE986";

type ObjectId = u32;
type OsStatus = i32;

#[repr(C)]
struct Address {
    selector: u32,
    scope: u32,
    element: u32,
}

const fn code(c: &[u8; 4]) -> u32 {
    u32::from_be_bytes(*c)
}

const SYSTEM_OBJECT: ObjectId = 1;
const SCOPE_GLOBAL: u32 = code(b"glob");
const ELEMENT_MAIN: u32 = 0;
const HARDWARE_DEVICES: u32 = code(b"dev#");
const DEVICE_UID: u32 = code(b"uid ");
const DEVICE_STREAMS: u32 = code(b"stm#");

#[repr(C)]
#[derive(Clone, Copy)]
struct CmTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const TIME_INVALID: CmTime = CmTime { value: 0, timescale: 0, flags: 0, epoch: 0 };
const TIME_VALID: u32 = 1;

#[repr(C)]
struct SampleTiming {
    duration: CmTime,
    presentation: CmTime,
    decode: CmTime,
}

type QueueAltered = unsafe extern "C" fn(ObjectId, *mut c_void, *mut c_void);

#[link(name = "CoreMediaIO", kind = "framework")]
unsafe extern "C" {
    fn CMIOObjectGetPropertyDataSize(
        object: ObjectId,
        address: *const Address,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: *mut u32,
    ) -> OsStatus;
    fn CMIOObjectGetPropertyData(
        object: ObjectId,
        address: *const Address,
        qualifier_size: u32,
        qualifier: *const c_void,
        size: u32,
        used: *mut u32,
        data: *mut c_void,
    ) -> OsStatus;
    fn CMIOStreamCopyBufferQueue(
        stream: ObjectId,
        altered: Option<QueueAltered>,
        refcon: *mut c_void,
        queue: *mut *mut c_void,
    ) -> OsStatus;
    fn CMIODeviceStartStream(device: ObjectId, stream: ObjectId) -> OsStatus;
    fn CMIODeviceStopStream(device: ObjectId, stream: ObjectId) -> OsStatus;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    fn CMSimpleQueueEnqueue(queue: *mut c_void, element: *const c_void) -> OsStatus;
    fn CMVideoFormatDescriptionCreateForImageBuffer(
        allocator: *const c_void,
        image: *const c_void,
        out: *mut *mut c_void,
    ) -> OsStatus;
    fn CMSampleBufferCreateForImageBuffer(
        allocator: *const c_void,
        image: *const c_void,
        data_ready: u8,
        make_ready: *const c_void,
        refcon: *mut c_void,
        format: *const c_void,
        timing: *const SampleTiming,
        out: *mut *mut c_void,
    ) -> OsStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(cf: *const c_void);
    fn CFGetTypeID(cf: *const c_void) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFURLCreateFromFileSystemRepresentation(
        allocator: *const c_void,
        path: *const u8,
        len: isize,
        directory: bool,
    ) -> *const c_void;
    fn CFBundleCreate(allocator: *const c_void, url: *const c_void) -> *const c_void;
    fn CFBundleGetValueForInfoDictionaryKey(bundle: *const c_void, key: *const c_void) -> *const c_void;
}

/// A CFString's text (borrowed reference).
unsafe fn text(s: *const c_void) -> Option<String> {
    if s.is_null() || unsafe { CFGetTypeID(s) != CFStringGetTypeID() } {
        return None;
    }
    Some(unsafe { &*(s as *const CFString) }.to_string())
}

fn property_size(object: ObjectId, selector: u32) -> u32 {
    let address = Address { selector, scope: SCOPE_GLOBAL, element: ELEMENT_MAIN };
    let mut size = 0;
    let status = unsafe { CMIOObjectGetPropertyDataSize(object, &address, 0, ptr::null(), &mut size) };
    if status == 0 { size } else { 0 }
}

/// An array property of object ids (devices, streams).
fn ids(object: ObjectId, selector: u32) -> Vec<ObjectId> {
    let size = property_size(object, selector);
    if size == 0 {
        return Vec::new();
    }
    let mut out = vec![0 as ObjectId; size as usize / std::mem::size_of::<ObjectId>()];
    let address = Address { selector, scope: SCOPE_GLOBAL, element: ELEMENT_MAIN };
    let mut used = 0;
    let status = unsafe {
        CMIOObjectGetPropertyData(object, &address, 0, ptr::null(), size, &mut used, out.as_mut_ptr().cast())
    };
    if status != 0 {
        return Vec::new();
    }
    out.truncate(used as usize / std::mem::size_of::<ObjectId>());
    out
}

/// A device's unique id.
fn device_uid(device: ObjectId) -> Option<String> {
    let address = Address { selector: DEVICE_UID, scope: SCOPE_GLOBAL, element: ELEMENT_MAIN };
    let mut uid: *const c_void = ptr::null();
    let mut used = 0;
    let status = unsafe {
        CMIOObjectGetPropertyData(
            device,
            &address,
            0,
            ptr::null(),
            std::mem::size_of::<*const c_void>() as u32,
            &mut used,
            (&mut uid as *mut *const c_void).cast(),
        )
    };
    if status != 0 || uid.is_null() {
        return None;
    }
    let s = unsafe { text(uid) };
    unsafe { CFRelease(uid) };
    s
}

/// The camera whose unique id is `uuid` among CoreMediaIO's devices.
fn find_camera(uuid: &str) -> Option<ObjectId> {
    ids(SYSTEM_OBJECT, HARDWARE_DEVICES)
        .into_iter()
        .find(|d| device_uid(*d).is_some_and(|u| u.eq_ignore_ascii_case(uuid)))
}

/// The OBS Virtual Camera's device UUID, from OBS's plug-in (beside Lightair's, or in OBS.app).
fn obs_camera_uuid() -> Option<String> {
    let beside =
        std::env::current_exe().ok().and_then(|e| Some(e.parent()?.parent()?.join("PlugIns/mac-virtualcam.plugin")));
    let places = beside
        .into_iter()
        .chain(std::iter::once("/Applications/OBS.app/Contents/PlugIns/mac-virtualcam.plugin".into()));
    let key = CFString::from_static_str("OBSCameraDeviceUUID");
    for place in places.filter(|p: &std::path::PathBuf| p.exists()) {
        let path = place.as_os_str().as_encoded_bytes();
        unsafe {
            let url = CFURLCreateFromFileSystemRepresentation(ptr::null(), path.as_ptr(), path.len() as isize, true);
            if url.is_null() {
                continue;
            }
            let bundle = CFBundleCreate(ptr::null(), url);
            CFRelease(url);
            if bundle.is_null() {
                continue;
            }
            let value = CFBundleGetValueForInfoDictionaryKey(bundle, CFRetained::as_ptr(&key).as_ptr().cast());
            let uuid = text(value);
            CFRelease(bundle);
            if uuid.is_some() {
                return uuid;
            }
        }
    }
    None
}

/// The sink queue's "it changed" callback: nothing to do (frames are only ever added).
unsafe extern "C" fn queue_altered(_: ObjectId, _: *mut c_void, _: *mut c_void) {}

/// The camera's sink stream, started: pictures sent here are what apps see.
pub struct Sink {
    device: ObjectId,
    stream: ObjectId,
    queue: *mut c_void,
    /// The pictures' format description (made from the first; every picture has its size).
    format: *mut c_void,
    size: (usize, usize),
}

// SAFETY: CoreMediaIO's queue and CoreMedia's descriptions are thread-safe CF objects; the
// sink is used by one thread at a time.
unsafe impl Send for Sink {}

impl Sink {
    /// Finds the camera and starts its sink stream.
    pub fn open() -> Result<Sink, String> {
        let device = find_camera(LIGHTAIR_CAMERA_UUID).or_else(|| obs_camera_uuid().and_then(|u| find_camera(&u)));
        let Some(device) = device else {
            return Err(
                "The virtual camera isn't installed. Open “Lightair Virtual Camera” in Applications to install it."
                    .into(),
            );
        };
        let streams = ids(device, DEVICE_STREAMS);
        // the second stream takes frames in (the first is what apps read)
        let Some(&stream) = streams.get(1) else {
            return Err("The virtual camera isn't ready. Try again in a moment.".into());
        };
        let mut queue: *mut c_void = ptr::null_mut();
        let queued = unsafe { CMIOStreamCopyBufferQueue(stream, Some(queue_altered), ptr::null_mut(), &mut queue) };
        let started =
            if queued == 0 && !queue.is_null() { unsafe { CMIODeviceStartStream(device, stream) } } else { -1 };
        if queued != 0 || queue.is_null() || started != 0 {
            tracing::warn!(stream, queued, started, "virtual camera: the sink stream didn't start");
            if !queue.is_null() {
                unsafe { CFRelease(queue) };
            }
            return Err("The virtual camera didn't start. Try again in a moment.".into());
        }
        Ok(Sink { device, stream, queue, format: ptr::null_mut(), size: (0, 0) })
    }

    /// Queues `picture` (BGRA), shown at `pts_ns` on the host clock.
    pub fn send(&mut self, picture: &CVPixelBuffer, pts_ns: i64) {
        let image = picture as *const CVPixelBuffer as *const c_void;
        let size =
            (objc2_core_video::CVPixelBufferGetWidth(picture), objc2_core_video::CVPixelBufferGetHeight(picture));
        if self.format.is_null() || self.size != size {
            if !self.format.is_null() {
                unsafe { CFRelease(self.format) };
                self.format = ptr::null_mut();
            }
            if unsafe { CMVideoFormatDescriptionCreateForImageBuffer(ptr::null(), image, &mut self.format) } != 0 {
                self.format = ptr::null_mut();
                return;
            }
            self.size = size;
        }
        let timing = SampleTiming {
            duration: TIME_INVALID,
            presentation: CmTime { value: pts_ns, timescale: 1_000_000_000, flags: TIME_VALID, epoch: 0 },
            decode: TIME_INVALID,
        };
        let mut sample: *mut c_void = ptr::null_mut();
        let made = unsafe {
            CMSampleBufferCreateForImageBuffer(
                ptr::null(),
                image,
                1,
                ptr::null(),
                ptr::null_mut(),
                self.format,
                &timing,
                &mut sample,
            )
        };
        if made != 0 || sample.is_null() {
            return;
        }
        // the queue takes the sample's reference; a full queue (no app reading) drops it
        if unsafe { CMSimpleQueueEnqueue(self.queue, sample) } != 0 {
            unsafe { CFRelease(sample) };
        }
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        unsafe {
            CMIODeviceStopStream(self.device, self.stream);
            CFRelease(self.queue);
            if !self.format.is_null() {
                CFRelease(self.format);
            }
        }
    }
}
