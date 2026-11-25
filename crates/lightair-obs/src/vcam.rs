//! The virtual camera on Windows.
//!
//! Lightair's own, "Lightair Virtual Camera" (Windows 11): a Media Foundation camera source
//! (`lightair_camera.dll`, crates/lightair-camera-win) that Windows' Frame Server runs for
//! any app that opens the camera; Lightair adds the camera with `MFCreateVirtualCamera` and its
//! `lightair_virtualcam` output (here) writes the program into the memory the camera reads.
//! Installing it is registering the DLL (`regsvr32`, as administrator: the Frame Server reads
//! its classes from HKLM).
//!
//! Where that can't be (Windows 10, or the DLL not registered), OBS's DirectShow camera ("OBS
//! Virtual Camera", `win-dshow`'s `virtualcam_output`) if it's installed (OBS's installer, or
//! its DLLs from libobs's `core\win-dshow\data`).
//!
//! On macOS the camera is a camera extension (modules/lightair-virtual-camera). libobs's
//! `lightair_virtualcam` output (lightair-effect-stack) sends it libobs's canvases; with
//! `LIGHTAIR_ENGINE=own`, [`mac::Sink`] sends it the compositor's frames instead
//! (`own_outputs::start_virtual_camera`).

/// The error when no camera is installed (the UI offers to install one).
pub const NOT_INSTALLED: &str = "The virtual camera isn't installed on this PC.";

#[cfg(windows)]
#[path = "../../lightair-camera-win/src/protocol.rs"]
#[allow(dead_code)]
mod protocol;

/// Which camera the virtual camera output sends to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Camera {
    /// Lightair Virtual Camera (macOS: the camera extension; Windows 11: lightair_camera.dll).
    Lightair,
    /// OBS Virtual Camera (Windows, where Lightair's can't be).
    Obs,
}

impl Camera {
    /// What other apps list it as.
    pub fn name(self) -> &'static str {
        match self {
            Camera::Lightair => "Lightair Virtual Camera",
            Camera::Obs => "OBS Virtual Camera",
        }
    }
}

/// The camera to use: Lightair's where it's installed, else OBS's; `None`: neither (Windows).
pub fn camera() -> Option<Camera> {
    #[cfg(windows)]
    {
        if win::testing() || win::lightair_installed() && win::can_add_cameras() {
            Some(Camera::Lightair)
        } else if win::obs_installed() {
            Some(Camera::Obs)
        } else {
            None
        }
    }
    #[cfg(not(windows))]
    Some(Camera::Lightair)
}

/// Whether a camera is installed.
pub fn installed() -> bool {
    camera().is_some()
}

/// Whether Lightair's own camera could be installed here but isn't (Windows 11 with the DLL
/// beside Lightair): the UI offers it even while OBS's works.
pub fn lightair_installable() -> bool {
    #[cfg(windows)]
    {
        !win::lightair_installed() && win::can_add_cameras() && win::dll().is_some()
    }
    #[cfg(not(windows))]
    false
}

/// Installs a camera, as administrator (Windows asks once): Lightair's on Windows 11, else
/// OBS's from libobs's install. Returns once the prompt is shown; whether it worked is
/// `installed()` afterwards.
#[cfg(windows)]
pub fn install() -> Result<(), String> {
    win::install()
}

#[cfg(not(windows))]
pub fn install() -> Result<(), String> {
    Err("The virtual camera installs with Lightair on this system.".into())
}

/// Registers the `lightair_virtualcam` output (Windows; on macOS it's lightair-effect-stack's).
pub(crate) fn register() {
    #[cfg(windows)]
    win::register();
}

/// macOS: sending pictures to the camera extension's sink stream directly, without libobs.
///
/// Sending needs no activation once the extension is installed: find the camera among
/// CoreMediaIO's devices by its device UUID (Lightair's, else the OBS Virtual Camera's from its
/// plug-in's Info.plist), open its sink stream (the device's second stream), and enqueue each
/// frame there as a sample buffer. A full queue (no app reading) drops the frame.
#[cfg(target_os = "macos")]
pub(crate) mod mac {
    use std::ffi::c_void;
    use std::ptr;

    use objc2_core_foundation::{CFRetained, CFString};
    use objc2_core_video::CVPixelBuffer;

    /// Frames a second the camera shows (its extension's rate).
    pub(crate) const FPS: u32 = 60;

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
        let beside = std::env::current_exe()
            .ok()
            .and_then(|e| Some(e.parent()?.parent()?.join("PlugIns/mac-virtualcam.plugin")));
        let places =
            beside.into_iter().chain(std::iter::once("/Applications/OBS.app/Contents/PlugIns/mac-virtualcam.plugin".into()));
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
    pub(crate) struct Sink {
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
        pub(crate) fn open() -> Result<Sink, String> {
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
            let started = if queued == 0 && !queue.is_null() { unsafe { CMIODeviceStartStream(device, stream) } } else { -1 };
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
        pub(crate) fn send(&mut self, picture: &CVPixelBuffer, pts_ns: i64) {
            let image = picture as *const CVPixelBuffer as *const c_void;
            let size = (
                objc2_core_video::CVPixelBufferGetWidth(picture),
                objc2_core_video::CVPixelBufferGetHeight(picture),
            );
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
}

#[cfg(windows)]
mod win {
    use super::protocol as p;
    use crate::sys;
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows_sys::Win32::System::Memory::{FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CLASSES_ROOT, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_64KEY, RRF_RT_REG_SZ, RegCloseKey, RegGetValueW, RegOpenKeyExW,
    };
    use windows_sys::Win32::System::SystemInformation::GetTickCount64;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// Tests only (`LIGHTAIR_VIRTUAL_CAMERA_TEST=<name>`): Lightair's output, without adding
    /// the camera to Windows, writing into `Local\<name>` (the test plays the camera's part,
    /// crates/lightair-preview's virtual_camera test).
    pub fn testing() -> bool {
        std::env::var_os("LIGHTAIR_VIRTUAL_CAMERA_TEST").is_some()
    }

    fn key_exists(root: HKEY, path: &str) -> bool {
        let key = wide(path);
        let mut out: HKEY = std::ptr::null_mut();
        let ok = unsafe { RegOpenKeyExW(root, key.as_ptr(), 0, KEY_READ | KEY_WOW64_64KEY, &mut out) } == 0;
        if ok {
            unsafe { RegCloseKey(out) };
        }
        ok
    }

    /// OBS's camera: its DirectShow filter's class.
    pub fn obs_installed() -> bool {
        key_exists(HKEY_CLASSES_ROOT, r"CLSID\{A3FCE0F5-3493-419F-958A-ABA1250EC20B}")
    }

    /// Lightair's camera: its class in HKLM, pointing at a DLL that's there.
    pub fn lightair_installed() -> bool {
        let key = wide(&format!(r"SOFTWARE\Classes\CLSID\{}\InprocServer32", p::CLSID_STRING));
        let mut buf = vec![0u16; 1024];
        let mut len = (buf.len() * 2) as u32;
        let r = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                std::ptr::null(),
                RRF_RT_REG_SZ | 0x0001_0000, /* RRF_SUBKEY_WOW6464KEY */
                std::ptr::null_mut(),
                buf.as_mut_ptr() as *mut c_void,
                &mut len,
            )
        };
        if r != 0 {
            return false;
        }
        let n = (len as usize / 2).saturating_sub(1);
        std::path::Path::new(&String::from_utf16_lossy(&buf[..n])).exists()
    }

    /// The camera DLL beside Lightair.
    pub fn dll() -> Option<std::path::PathBuf> {
        let d = std::env::current_exe().ok()?.parent()?.join("lightair_camera.dll");
        d.exists().then_some(d)
    }

    type CreateVirtualCamera =
        unsafe extern "system" fn(i32, i32, i32, *const u16, *const u16, *const windows_sys::core::GUID, u32, *mut *mut c_void) -> i32;

    /// `MFCreateVirtualCamera` (Windows 11 and later).
    fn create_virtual_camera() -> Option<CreateVirtualCamera> {
        static F: std::sync::OnceLock<Option<CreateVirtualCamera>> = std::sync::OnceLock::new();
        *F.get_or_init(find_create_virtual_camera)
    }

    fn find_create_virtual_camera() -> Option<CreateVirtualCamera> {
        let lib = wide("mfsensorgroup.dll");
        let module = unsafe { LoadLibraryW(lib.as_ptr()) };
        if module.is_null() {
            return None;
        }
        let f = unsafe { GetProcAddress(module, c"MFCreateVirtualCamera".as_ptr() as *const u8) }?;
        Some(unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, CreateVirtualCamera>(f) })
    }

    pub fn can_add_cameras() -> bool {
        create_virtual_camera().is_some()
    }

    /// Whether Windows lists a camera called `name` (Media Foundation's video capture devices).
    fn camera_listed(name: &str) -> bool {
        use windows::Win32::Media::MediaFoundation::*;
        use windows::Win32::System::Com::CoTaskMemFree;
        let mut attrs = None;
        if unsafe { MFCreateAttributes(&mut attrs, 1) }.is_err() {
            return false;
        }
        let Some(attrs) = attrs else { return false };
        if unsafe { attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID) }.is_err() {
            return false;
        }
        let (mut list, mut count) = (std::ptr::null_mut(), 0u32);
        if unsafe { MFEnumDeviceSources(&attrs, &mut list, &mut count) }.is_err() || list.is_null() {
            return false;
        }
        let devices = unsafe { std::slice::from_raw_parts_mut(list, count as usize) };
        let mut found = false;
        for d in devices.iter_mut() {
            if let Some(a) = d.take() {
                let (mut s, mut n) = (windows::core::PWSTR::null(), 0u32);
                if unsafe { a.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut s, &mut n) }.is_ok() {
                    // (as added; or as Windows lists it: "<name> (Windows Virtual Camera)")
                    found |= unsafe { s.to_string() }.is_ok_and(|have| {
                        tracing::debug!(%have, "camera");
                        have == name || have.strip_prefix(name).is_some_and(|rest| rest.starts_with(" ("))
                    });
                    unsafe { CoTaskMemFree(Some(s.0 as *const c_void)) };
                }
            }
        }
        unsafe { CoTaskMemFree(Some(list as *const c_void)) };
        found
    }

    /// Adds Lightair's camera for this user (it stays, across restarts, like a webcam) and turns
    /// it on, unless Windows lists it already.
    fn add_camera() -> Result<(), String> {
        use windows::Win32::Media::MediaFoundation::{IMFVirtualCamera, MFSTARTUP_FULL, MFStartup};
        use windows::core::Interface;
        let create = create_virtual_camera().ok_or("This version of Windows can't add cameras (Windows 11 can).")?;
        unsafe { MFStartup(0x0002_0070, MFSTARTUP_FULL) }.map_err(|e| format!("Media Foundation didn't start: {e}"))?;
        if camera_listed(p::NAME) {
            return Ok(());
        }
        let (name, id) = (wide(p::NAME), wide(p::CLSID_STRING));
        let mut raw: *mut c_void = std::ptr::null_mut();
        // software camera source, lasting (system), for the current user
        let hr = unsafe { create(0, 1, 0, name.as_ptr(), id.as_ptr(), std::ptr::null(), 0, &mut raw) };
        if hr < 0 || raw.is_null() {
            return Err(format!("Windows didn't add the camera ({:#010x}).", hr as u32));
        }
        let camera = unsafe { IMFVirtualCamera::from_raw(raw) };
        unsafe { camera.Start(None) }.map_err(|e| format!("Windows didn't turn the camera on: {e}"))?;
        Ok(())
    }

    pub fn install() -> Result<(), String> {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
        let commands: Vec<String> = match dll().filter(|_| can_add_cameras()) {
            // Lightair's
            Some(d) => vec![format!("regsvr32.exe /s \"{}\"", d.display())],
            // OBS's, from libobs's install
            None => {
                let runtime = crate::runtime::Runtime::detect()?;
                let root = runtime.frameworks.parent().and_then(std::path::Path::parent).ok_or("libobs has no install folder")?;
                let data = root.join(r"core\win-dshow\data");
                let dlls: Vec<_> = ["obs-virtualcam-module64.dll", "obs-virtualcam-module32.dll"]
                    .into_iter()
                    .map(|d| data.join(d))
                    .filter(|d| d.exists())
                    .collect();
                if dlls.is_empty() {
                    return Err(format!("The virtual camera's files aren't in {}.", data.display()));
                }
                dlls.iter().map(|d| format!("regsvr32.exe /i /s \"{}\"", d.display())).collect()
            }
        };
        let args = format!("/c {}", commands.join(" & "));
        let (verb, file, params) = (wide("runas"), wide("cmd.exe"), wide(&args));
        // (above 32: started; the user may still say no to the prompt)
        let r = unsafe { ShellExecuteW(std::ptr::null_mut(), verb.as_ptr(), file.as_ptr(), params.as_ptr(), std::ptr::null(), SW_HIDE) };
        if r as isize > 32 { Ok(()) } else { Err("Windows didn't start the installation.".into()) }
    }

    // ---- the output ----

    /// The camera's memory, open while an app has the camera open (the camera makes it).
    struct Memory {
        handle: HANDLE,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
    }

    impl Memory {
        fn open() -> Option<Self> {
            let test = std::env::var("LIGHTAIR_VIRTUAL_CAMERA_TEST").ok().map(|n| format!(r"Local\{n}"));
            let names = match &test {
                Some(t) => vec![t.as_str()],
                None => vec![p::MEMORY_GLOBAL, p::MEMORY_LOCAL],
            };
            for name in names {
                let n = wide(name);
                let handle = unsafe { OpenFileMappingW(FILE_MAP_READ | FILE_MAP_WRITE, 0, n.as_ptr()) };
                if handle.is_null() {
                    continue;
                }
                let view = unsafe { MapViewOfFile(handle, FILE_MAP_READ | FILE_MAP_WRITE, 0, 0, p::SIZE) };
                if view.Value.is_null() {
                    unsafe { CloseHandle(handle) };
                    continue;
                }
                let m = Self { handle, view };
                let h = m.header();
                if h.magic == p::MAGIC && h.version == p::VERSION && (h.width, h.height) == (p::WIDTH, p::HEIGHT) {
                    return Some(m);
                }
            }
            None
        }

        fn header(&self) -> &p::Header {
            unsafe { &*(self.view.Value as *const p::Header) }
        }

        /// Whether an app is reading the camera (it reads each frame it sends; when Lightair
        /// first writes, `read_ms` is from the camera's opening, which is recent too).
        fn read_lately(&self) -> bool {
            unsafe { GetTickCount64() }.saturating_sub(self.header().read_ms.load(Ordering::Relaxed)) < 2000
        }

        /// Writes an NV12 frame (`planes`: Y and UV, with their strides).
        unsafe fn write(&self, y: (*const u8, usize), uv: (*const u8, usize)) {
            let h = self.header();
            let (w, rows) = (p::WIDTH as usize, p::HEIGHT as usize);
            let dst = unsafe { (self.view.Value as *mut u8).add(p::HEADER_SIZE) };
            h.seq.fetch_add(1, Ordering::AcqRel);
            for r in 0..rows {
                unsafe { std::ptr::copy_nonoverlapping(y.0.add(r * y.1), dst.add(r * w), w) };
            }
            let uv_dst = unsafe { dst.add(w * rows) };
            for r in 0..rows / 2 {
                unsafe { std::ptr::copy_nonoverlapping(uv.0.add(r * uv.1), uv_dst.add(r * w), w) };
            }
            h.seq.fetch_add(1, Ordering::Release);
            h.written_ms.store(unsafe { GetTickCount64() }, Ordering::Release);
        }
    }

    impl Drop for Memory {
        fn drop(&mut self) {
            unsafe {
                UnmapViewOfFile(self.view);
                CloseHandle(self.handle);
            }
        }
    }

    struct Output {
        output: *mut sys::obs_output_t,
        /// Used by `raw_video` on libobs's video thread, which can still be running as `stop`
        /// returns (libobs ends a capture on a thread of its own): kept until the output goes.
        state: std::sync::Mutex<State>,
    }

    #[derive(Default)]
    struct State {
        memory: Option<Memory>,
        tried: Option<Instant>,
        /// When a frame was last written: the camera shows 30 a second, so no more are copied.
        wrote: Option<Instant>,
    }

    unsafe impl Send for Memory {}

    unsafe extern "C" fn get_name(_: *mut c_void) -> *const std::ffi::c_char {
        c"Lightair virtual camera".as_ptr()
    }

    unsafe extern "C" fn create(_: *mut sys::obs_data_t, output: *mut sys::obs_output_t) -> *mut c_void {
        Box::into_raw(Box::new(Output { output, state: Default::default() })) as *mut c_void
    }

    unsafe extern "C" fn destroy(data: *mut c_void) {
        drop(unsafe { Box::from_raw(data as *mut Output) });
    }

    unsafe extern "C" fn start(data: *mut c_void) -> bool {
        let o = unsafe { &mut *(data as *mut Output) };
        if let Err(e) = if testing() { Ok(()) } else { add_camera() } {
            tracing::warn!(%e, "Lightair's virtual camera");
            let msg = std::ffi::CString::new(e).unwrap_or_default();
            unsafe { sys::obs_output_set_last_error(o.output, msg.as_ptr()) };
            return false;
        }
        // the camera's picture: 1080p NV12, BT.601 limited (libobs scales and converts the
        // canvas to it; protocol.rs)
        let conversion = sys::video_scale_info {
            format: sys::VIDEO_FORMAT_NV12,
            width: p::WIDTH,
            height: p::HEIGHT,
            range: sys::VIDEO_RANGE_PARTIAL,
            colorspace: sys::VIDEO_CS_601,
        };
        unsafe {
            sys::obs_output_set_video_conversion(o.output, &conversion);
            if !sys::obs_output_can_begin_data_capture(o.output, 0) {
                return false;
            }
            // (the memory found last time is kept: the camera's, still)
            o.state.lock().unwrap().tried = None;
            sys::obs_output_begin_data_capture(o.output, 0)
        }
    }

    unsafe extern "C" fn stop(data: *mut c_void, _: u64) {
        let o = unsafe { &*(data as *const Output) };
        unsafe { sys::obs_output_end_data_capture(o.output) };
    }

    unsafe extern "C" fn raw_video(data: *mut c_void, frame: *mut sys::video_data) {
        let out = unsafe { &*(data as *const Output) };
        let mut guard = out.state.lock().unwrap();
        let o = &mut *guard;
        if o.memory.is_none() {
            // the camera's memory is there once an app opens the camera: looked for twice a second
            if o.tried.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) {
                return;
            }
            o.tried = Some(Instant::now());
            o.memory = Memory::open();
            if o.memory.is_some() {
                tracing::info!("an app opened Lightair's virtual camera");
            }
        }
        let (Some(m), Some(f)) = (&o.memory, unsafe { frame.as_ref() }) else { return };
        // only frames at the camera's size (the conversion asked for), rows as wide as its
        if f.data[0].is_null() || f.data[1].is_null() || (f.linesize[0] as usize) < p::WIDTH as usize || (f.linesize[1] as usize) < p::WIDTH as usize {
            return;
        }
        // (the size libobs scales to: the conversion asked for as the output started)
        let conversion = unsafe { sys::obs_output_get_video_conversion(out.output).as_ref() };
        if !conversion.is_some_and(|c| (c.width, c.height, c.format) == (p::WIDTH, p::HEIGHT, sys::VIDEO_FORMAT_NV12)) {
            return;
        }
        // nothing copied while no app has the camera open, nor faster than it shows frames
        let period = Duration::from_millis(1000 / p::FPS as u64 - 3);
        if !m.read_lately() || o.wrote.is_some_and(|t| t.elapsed() < period) {
            return;
        }
        o.wrote = Some(Instant::now());
        unsafe { m.write((f.data[0], f.linesize[0] as usize), (f.data[1], f.linesize[1] as usize)) };
    }

    pub fn register() {
        let info = sys::obs_output_info {
            id: c"lightair_virtualcam".as_ptr(),
            flags: sys::OBS_OUTPUT_VIDEO,
            get_name: Some(get_name),
            create: Some(create),
            destroy: Some(destroy),
            start: Some(start),
            stop: Some(stop),
            raw_video: Some(raw_video),
            ..Default::default()
        };
        unsafe { sys::obs_register_output_s(&info, std::mem::size_of::<sys::obs_output_info>()) };
    }
}
