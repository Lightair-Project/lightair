//! The virtual camera: on macOS the camera extension (modules/lightair-virtual-camera), sent the
//! compositor's frames by [`crate::vcam_mac::Sink`] (`own_outputs::start_virtual_camera`).
//!
//! On Windows 11, Lightair's own camera: a Media Foundation camera source
//! (`lightair_camera.dll`, crates/lightair-camera-win) that Windows' Frame Server runs for any
//! app that opens the camera. Lightair adds the camera with `MFCreateVirtualCamera` and writes
//! the program into the memory the camera reads ([`win::Sink`]). Installing it is registering
//! the DLL (`regsvr32`, as administrator: the Frame Server reads its classes from HKLM).

/// The error when no camera is installed (the UI offers to install one).
pub const NOT_INSTALLED: &str = "The virtual camera isn't installed on this PC.";

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

/// The camera to use; `None`: none here (Windows without Lightair's camera installed, or before
/// Windows 11).
pub fn camera() -> Option<Camera> {
    #[cfg(windows)]
    {
        (win::testing() || win::lightair_installed() && win::can_add_cameras()).then_some(Camera::Lightair)
    }
    #[cfg(target_os = "macos")]
    {
        Some(Camera::Lightair)
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    None
}

/// Whether a camera is installed.
pub fn installed() -> bool {
    camera().is_some()
}

/// Whether Lightair's own camera could be installed here but isn't (Windows 11 with the DLL
/// beside Lightair).
pub fn lightair_installable() -> bool {
    #[cfg(windows)]
    {
        !win::lightair_installed() && win::can_add_cameras() && win::dll().is_some()
    }
    #[cfg(not(windows))]
    false
}

/// Installs Lightair's camera, as administrator (Windows asks once). Returns once the prompt is
/// shown; whether it worked is `installed()` afterwards.
#[cfg(windows)]
pub fn install() -> Result<(), String> {
    win::install()
}

/// Installs a camera (it comes with Lightair on macOS).
#[cfg(not(windows))]
pub fn install() -> Result<(), String> {
    Err("The virtual camera installs with Lightair on this system.".into())
}

#[cfg(windows)]
#[path = "../../lightair-camera-win/src/protocol.rs"]
#[allow(dead_code)]
mod protocol;

/// Windows: Lightair's camera, found, added and installed, and the program written into its
/// memory.
#[cfg(windows)]
pub mod win {
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows_sys::Win32::System::Memory::{
        FILE_MAP_READ, FILE_MAP_WRITE, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile,
    };
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    use windows_sys::Win32::System::SystemInformation::GetTickCount64;

    use super::protocol as p;

    /// The pictures the camera takes: its own size, at most its rate (NV12, BT.601 limited).
    pub const SIZE: Option<(u32, u32)> = Some((p::WIDTH, p::HEIGHT));
    pub const FPS: u32 = p::FPS;

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// Tests only (`LIGHTAIR_VIRTUAL_CAMERA_TEST=<name>`): Lightair's output, without adding
    /// the camera to Windows, writing into `Local\<name>` (the test plays the camera's part).
    pub fn testing() -> bool {
        std::env::var_os("LIGHTAIR_VIRTUAL_CAMERA_TEST").is_some()
    }

    /// Lightair's camera: its class in HKLM, pointing at a DLL that's there.
    pub fn lightair_installed() -> bool {
        let key = wide(&format!(r"SOFTWARE\Classes\CLSID\{}\InprocServer32", p::CLSID_STRING));
        let mut buf = vec![0u16; 1024];
        let mut len = (buf.len() * 2) as u32;
        // RRF_SUBKEY_WOW6464KEY: the 64-bit view
        let r = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                std::ptr::null(),
                RRF_RT_REG_SZ | 0x0001_0000,
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

    type CreateVirtualCamera = unsafe extern "system" fn(
        i32,
        i32,
        i32,
        *const u16,
        *const u16,
        *const windows_sys::core::GUID,
        u32,
        *mut *mut c_void,
    ) -> i32;

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
        // SAFETY: MFCreateVirtualCamera has this signature.
        Some(unsafe { std::mem::transmute::<unsafe extern "system" fn() -> isize, CreateVirtualCamera>(f) })
    }

    /// Whether this Windows can add cameras (Windows 11).
    pub fn can_add_cameras() -> bool {
        create_virtual_camera().is_some()
    }

    /// Whether Windows lists a camera called `name` (Media Foundation's video capture devices).
    fn camera_listed(name: &str) -> bool {
        use windows::Win32::Media::MediaFoundation::{
            MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE,
            MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID, MFCreateAttributes, MFEnumDeviceSources,
        };
        use windows::Win32::System::Com::CoTaskMemFree;
        let mut attrs = None;
        if unsafe { MFCreateAttributes(&mut attrs, 1) }.is_err() {
            return false;
        }
        let Some(attrs) = attrs else { return false };
        if unsafe {
            attrs.SetGUID(&MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE, &MF_DEVSOURCE_ATTRIBUTE_SOURCE_TYPE_VIDCAP_GUID)
        }
        .is_err()
        {
            return false;
        }
        let (mut list, mut count) = (std::ptr::null_mut(), 0u32);
        if unsafe { MFEnumDeviceSources(&attrs, &mut list, &mut count) }.is_err() || list.is_null() {
            return false;
        }
        // SAFETY: Media Foundation gave `count` activation objects at `list`, ours to release.
        let devices = unsafe { std::slice::from_raw_parts_mut(list, count as usize) };
        let mut found = false;
        for d in devices.iter_mut() {
            if let Some(a) = d.take() {
                let (mut s, mut n) = (windows::core::PWSTR::null(), 0u32);
                if unsafe { a.GetAllocatedString(&MF_DEVSOURCE_ATTRIBUTE_FRIENDLY_NAME, &mut s, &mut n) }.is_ok() {
                    // as added, or as Windows lists it: "<name> (Windows Virtual Camera)"
                    found |= unsafe { s.to_string() }
                        .is_ok_and(|have| have == name || have.strip_prefix(name).is_some_and(|r| r.starts_with(" (")));
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
        use windows::Win32::Media::MediaFoundation::{IMFVirtualCamera, MF_VERSION, MFSTARTUP_FULL, MFStartup};
        use windows::core::Interface;
        let create = create_virtual_camera().ok_or("This version of Windows can't add cameras (Windows 11 can).")?;
        unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|e| format!("Media Foundation didn't start: {e}"))?;
        if camera_listed(p::NAME) {
            return Ok(());
        }
        let (name, id) = (wide(p::NAME), wide(p::CLSID_STRING));
        let mut raw: *mut c_void = std::ptr::null_mut();
        // a software camera source, lasting (system), for the current user
        let hr = unsafe { create(0, 1, 0, name.as_ptr(), id.as_ptr(), std::ptr::null(), 0, &mut raw) };
        if hr < 0 || raw.is_null() {
            return Err(format!("Windows didn't add the camera ({:#010x}).", hr as u32));
        }
        // SAFETY: MFCreateVirtualCamera made it and handed its reference over.
        let camera = unsafe { IMFVirtualCamera::from_raw(raw) };
        unsafe { camera.Start(None) }.map_err(|e| format!("Windows didn't turn the camera on: {e}"))?;
        Ok(())
    }

    /// Registers the camera DLL beside Lightair, as administrator.
    pub fn install() -> Result<(), String> {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_HIDE;
        if !can_add_cameras() {
            return Err("Lightair's virtual camera needs Windows 11.".into());
        }
        let dll = dll().ok_or("The virtual camera's file (lightair_camera.dll) isn't beside Lightair.")?;
        let args = format!("/s \"{}\"", dll.display());
        let (verb, file, params) = (wide("runas"), wide("regsvr32.exe"), wide(&args));
        // above 32: started (the user may still say no to the prompt)
        let r = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                file.as_ptr(),
                params.as_ptr(),
                std::ptr::null(),
                SW_HIDE,
            )
        };
        if r as isize > 32 { Ok(()) } else { Err("Windows didn't start the installation.".into()) }
    }

    /// The camera's memory, there while an app has the camera open (the camera makes it).
    struct Memory {
        handle: HANDLE,
        view: MEMORY_MAPPED_VIEW_ADDRESS,
    }

    // SAFETY: a file mapping and its view may be used from any thread.
    unsafe impl Send for Memory {}

    impl Memory {
        fn open() -> Option<Memory> {
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
                let m = Memory { handle, view };
                let h = m.header();
                if h.magic == p::MAGIC && h.version == p::VERSION && (h.width, h.height) == (p::WIDTH, p::HEIGHT) {
                    return Some(m);
                }
            }
            None
        }

        fn header(&self) -> &p::Header {
            // SAFETY: the view is at least `p::SIZE` bytes, the header first.
            unsafe { &*(self.view.Value as *const p::Header) }
        }

        /// Whether an app is reading the camera (it reads each frame it shows; when Lightair
        /// first writes, `read_ms` is from the camera's opening, which is recent too).
        fn read_lately(&self) -> bool {
            unsafe { GetTickCount64() }.saturating_sub(self.header().read_ms.load(Ordering::Relaxed)) < 2000
        }

        /// Writes an NV12 frame of the camera's size: `stride` bytes from row to row, the chroma
        /// rows after the luma rows.
        fn write(&self, data: &[u8], stride: usize) {
            let (w, rows) = (p::WIDTH as usize, p::HEIGHT as usize);
            if stride < w || data.len() < stride * (rows + rows / 2) {
                return;
            }
            let h = self.header();
            // SAFETY: the frame follows the header and is `p::FRAME_SIZE` bytes: `rows` rows of
            // `w` luma bytes, then `rows / 2` rows of `w` chroma bytes.
            let dst = unsafe {
                std::slice::from_raw_parts_mut((self.view.Value as *mut u8).add(p::HEADER_SIZE), p::FRAME_SIZE)
            };
            h.seq.fetch_add(1, Ordering::AcqRel);
            for (r, row) in dst.chunks_exact_mut(w).enumerate() {
                row.copy_from_slice(&data[r * stride..r * stride + w]);
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

    /// Sends the program to Lightair's camera: each picture written into the camera's memory
    /// while an app has the camera open.
    pub struct Sink {
        memory: Option<Memory>,
        /// When the camera's memory was last looked for.
        tried: Option<Instant>,
    }

    impl Sink {
        /// Adds the camera to Windows (unless it's there) and starts sending.
        pub fn open() -> Result<Sink, String> {
            if !testing() {
                if !lightair_installed() {
                    return Err(super::NOT_INSTALLED.into());
                }
                add_camera()?;
            }
            Ok(Sink { memory: None, tried: None })
        }

        /// Writes a picture for the camera: NV12 (BT.601, limited range) of `size`, which must
        /// be the camera's, `stride` bytes from row to row, the chroma rows after the luma
        /// rows. The tap's thread.
        pub fn send(&mut self, nv12: &[u8], stride: usize, size: (u32, u32), _pts_ns: i64) {
            if size != (p::WIDTH, p::HEIGHT) {
                return;
            }
            if self.memory.is_none() {
                // the camera's memory is there once an app opens the camera: looked for twice a
                // second
                if self.tried.is_some_and(|t| t.elapsed() < Duration::from_millis(500)) {
                    return;
                }
                self.tried = Some(Instant::now());
                self.memory = Memory::open();
                if self.memory.is_some() {
                    tracing::info!("an app opened Lightair's virtual camera");
                }
            }
            let Some(m) = &self.memory else { return };
            // nothing written while no app has the camera open
            if m.read_lately() {
                m.write(nv12, stride);
            }
        }
    }
}
