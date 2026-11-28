//! The Windows sandbox for a helper (plugins.md §4.8, §4.11).
//!
//! - **The process:** an AppContainer of its own (`Lightair.Helper.<id>`), with no capabilities
//!   unless a granted hole needs one. An AppContainer token is low integrity, so the helper can't
//!   send input to or open other apps' windows and processes. It runs in a job object that kills
//!   it when Lightair closes it (or exits), allows no breaking away, caps its memory and CPU and
//!   takes the clipboard and desktop settings away; it may not start other programs (the child
//!   process policy). Only its three pipes are passed to it.
//! - **Files:** its AppContainer SID is granted reading of the package folder, reading and
//!   writing of its data folder, and what the user picked; nothing else of the user's grants it.
//!   The picked grants are taken back when it stops.
//! - **Network:** an AppContainer without capabilities reaches no network at all, and can't
//!   reach Lightair's proxy on loopback either (a loopback exemption needs admin rights). So a
//!   bridge, `lightair-sandbox.exe bridge <pipe>`, runs in the same AppContainer and job: it
//!   listens on 127.0.0.1 inside the container (loopback within one AppContainer is allowed),
//!   and carries each connection over a named pipe to Lightair, which passes it to the helper's
//!   proxy lane. The pipe admits only that AppContainer. The helper gets the bridge's address as
//!   `HTTPS_PROXY` and the like, exactly as on macOS.
//! - **Listening:** apps outside can't connect into an AppContainer either. A helper listens on
//!   an inside port Lightair gives it (`LIGHTAIR_LISTEN_<port>`, `LIGHTAIR_LISTEN_ANY`); while it
//!   does, Lightair holds the outside port and passes each connection through the bridge to it.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::{size_of, zeroed};
use std::net::TcpStream;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use windows_sys::Win32::Foundation::{
    CloseHandle, ERROR_IO_PENDING, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, GENERIC_READ, GENERIC_WRITE, GetLastError,
    HANDLE, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE, LocalFree, SetHandleInformation,
};
use windows_sys::Win32::NetworkManagement::IpHelper::{
    GetExtendedTcpTable, MIB_TCPTABLE_OWNER_PID, TCP_TABLE_OWNER_PID_LISTENER,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, EXPLICIT_ACCESS_W, GRANT_ACCESS,
    GetNamedSecurityInfoW, GetSecurityInfo, NO_MULTIPLE_TRUSTEE, REVOKE_ACCESS, SDDL_REVISION_1, SE_FILE_OBJECT,
    SE_KERNEL_OBJECT, SetEntriesInAclW, SetNamedSecurityInfoW, SetSecurityInfo, TRUSTEE_IS_SID,
    TRUSTEE_IS_WELL_KNOWN_GROUP, TRUSTEE_W,
};
use windows_sys::Win32::Security::Isolation::{CreateAppContainerProfile, DeriveAppContainerSidFromAppContainerName};
use windows_sys::Win32::Security::{
    ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, CONTAINER_INHERIT_ACE, CopySid, DACL_SECURITY_INFORMATION,
    DeriveCapabilitySidsFromName, EqualSid, FreeSid, GetAce, GetLengthSid, GetSecurityDescriptorSacl,
    LABEL_SECURITY_INFORMATION, NO_INHERITANCE, OBJECT_INHERIT_ACE, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
    SECURITY_CAPABILITIES, SID_AND_ATTRIBUTES, SUB_CONTAINERS_AND_OBJECTS_INHERIT,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, DELETE, FILE_DELETE_CHILD, FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_FLAG_OVERLAPPED, FILE_GENERIC_EXECUTE,
    FILE_GENERIC_READ, FILE_GENERIC_WRITE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX, READ_CONTROL, ReadFile, WRITE_DAC,
    WRITE_OWNER, WriteFile,
};
use windows_sys::Win32::System::IO::{CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_JOB_MEMORY, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_DISPLAYSETTINGS, JOB_OBJECT_UILIMIT_EXITWINDOWS,
    JOB_OBJECT_UILIMIT_GLOBALATOMS, JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
    JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_BASIC_PROCESS_ID_LIST, JOBOBJECT_BASIC_UI_RESTRICTIONS, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation, JobObjectBasicProcessIdList,
    JobObjectBasicUIRestrictions, JobObjectCpuRateControlInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, SetInformationJobObject,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, CreatePipe, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    PIPE_UNLIMITED_INSTANCES, PIPE_WAIT, WaitNamedPipeW,
};
use windows_sys::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateEventW, CreateProcessW, DETACHED_PROCESS, DeleteProcThreadAttributeList,
    EXTENDED_STARTUPINFO_PRESENT, InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcess,
    PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST,
    PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES, PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW, STARTF_USESTDHANDLES, STARTUPINFOEXW, UpdateProcThreadAttribute,
};
use windows_sys::Win32::System::WindowsProgramming::PROCESS_CREATION_CHILD_PROCESS_RESTRICTED;

use crate::grants::{Device, Grants};
use crate::proxy::{Event, Lane, Rules};
use crate::spawn::Child;
use crate::{Helper, Sender, Spec};

/// A helper's memory cap (all its processes together).
const MEMORY_CAP: usize = 4 << 30;
/// Its CPU cap: this share of the whole machine, in hundredths of a percent.
const CPU_CAP: u32 = 50 * 100;

const READ: u32 = FILE_GENERIC_READ | FILE_GENERIC_EXECUTE;
const READ_WRITE: u32 = FILE_GENERIC_READ | FILE_GENERIC_WRITE | FILE_GENERIC_EXECUTE | DELETE | FILE_DELETE_CHILD;

/// What a running sandboxed helper holds besides its process: dropped after the process is.
pub(crate) struct Sandbox {
    job: Job,
    sid: Sid,
    /// Picked paths granted for this run, taken back when it stops.
    picked: Vec<PathBuf>,
    bridge: Option<Bridge>,
    listening: Option<Listening>,
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        // the job, closed after this, kills whatever of the helper is left
        if let Some(l) = &mut self.listening {
            l.stop();
        }
        if let Some(b) = &mut self.bridge {
            b.stop();
        }
        for p in &self.picked {
            if let Err(e) = revoke(p, &self.sid) {
                tracing::warn!("couldn't take back the helper's grant of {}: {e}", p.display());
            }
        }
    }
}

impl Sandbox {
    /// CPU seconds and peak memory (MB) used so far by all of the helper's processes.
    pub(crate) fn usage(&self) -> Option<(f64, f64)> {
        unsafe {
            let mut acct: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = zeroed();
            let mut ext: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
            let h = self.job.0.as_raw_handle() as HANDLE;
            if QueryInformationJobObject(
                h,
                JobObjectBasicAccountingInformation,
                &mut acct as *mut _ as _,
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                null_mut(),
            ) == 0
                || QueryInformationJobObject(
                    h,
                    JobObjectExtendedLimitInformation,
                    &mut ext as *mut _ as _,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                    null_mut(),
                ) == 0
            {
                return None;
            }
            let cpu = (acct.TotalUserTime + acct.TotalKernelTime) as f64 / 1e7;
            Some((cpu, ext.PeakJobMemoryUsed as f64 / 1048576.0))
        }
    }

    /// The AppContainer's SID as text (`S-1-15-2-…`), for logs.
    pub(crate) fn sid_string(&self) -> String {
        self.sid.to_string()
    }
}

/// Starts a sandboxed helper. `launcher` is `lightair-sandbox.exe` beside Lightair's executable
/// (the network bridge).
pub(crate) fn launch(spec: &Spec, launcher: &Path, events: Sender<Event>) -> io::Result<Helper> {
    let g = &spec.grants;
    let sid = container(&spec.id)?;
    let caps = capabilities(g).map_err(ctx("its capabilities"))?;

    // files: its package to read, its data folder to read and write, what the user picked
    grant(&spec.package_dir, &sid, READ).map_err(ctx("granting its package folder"))?;
    grant(&spec.data_dir, &sid, READ_WRITE).map_err(ctx("granting its data folder"))?;
    let mut picked = Vec::new();
    for p in &g.picked {
        grant(&p.path, &sid, if p.write { READ_WRITE } else { READ }).map_err(ctx("granting a picked path"))?;
        picked.push(p.path.clone());
    }
    for name in &g.ipc {
        if let Err(e) = open_pipe(name, &sid) {
            tracing::warn!("helper {}: ipc:{name} not opened: {e}", spec.id);
        }
    }

    let job = Job::new().map_err(ctx("its job object"))?;
    let mut env = base_env(spec);
    let (listens, listen_env) = listen_ports(g).map_err(ctx("its listening ports"))?;
    env.extend(listen_env);
    let lane = if g.uses_network() { Some(Lane::open(&spec.id, Rules::from(g), events.clone())?) } else { None };
    let bridge = if lane.is_some() || !listens.is_empty() {
        let launcher = exe_path(launcher);
        grant_file_read(&launcher, &sid).map_err(ctx("granting the bridge's program"))?;
        let (bridge, port) =
            Bridge::start(&spec.id, &launcher, &sid, &job, lane.as_ref().map(Lane::port), &spec.data_dir)
                .map_err(ctx("its network bridge"))?;
        if lane.is_some() {
            let url = format!("http://127.0.0.1:{port}");
            for k in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY", "https_proxy", "http_proxy", "all_proxy"] {
                env.push((k.into(), url.clone()));
            }
            env.push(("NO_PROXY".into(), String::new()));
        }
        Some(bridge)
    } else {
        None
    };

    let mut sandbox = Sandbox { job, sid, picked, bridge, listening: None };
    let child = spawn_contained(&spec.entry, &spec.args, &spec.data_dir, &env, &sandbox.sid, &caps, &sandbox.job)
        .inspect_err(|_| {
            if let Some(b) = &mut sandbox.bridge {
                b.stop();
            }
        })
        .map_err(ctx("starting it in its AppContainer"))?;
    if let (false, Some(b)) = (listens.is_empty(), &sandbox.bridge) {
        // on the LAN too when the home network is granted
        let listening = Listening::start(&spec.id, listens, sandbox.job.handle(), b.shared.clone(), events, g.lan);
        sandbox.listening = Some(listening.map_err(ctx("watching its listening ports"))?);
    }
    tracing::info!("helper {} started in AppContainer {}", spec.id, sandbox.sid_string());
    Ok(Helper { child, lane, profile_file: None, sandbox: Some(sandbox) })
}

/// Prefixes an error with the step that failed.
fn ctx(step: &'static str) -> impl Fn(io::Error) -> io::Error {
    move |e| io::Error::new(e.kind(), format!("{step}: {e}"))
}

/// A sandboxed helper's environment on Windows: nothing of Lightair's own, its data folder as
/// its home and temporary folder.
fn base_env(spec: &Spec) -> Vec<(String, String)> {
    let data = plain(&spec.data_dir);
    let tmp = plain(&spec.data_dir.join("tmp"));
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let mut env = vec![
        ("SystemRoot".to_string(), root.clone()),
        ("windir".to_string(), root.clone()),
        ("SystemDrive".to_string(), std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into())),
        ("PATH".to_string(), format!(r"{root}\System32;{root}")),
        ("PATHEXT".to_string(), ".COM;.EXE".to_string()),
        ("USERPROFILE".to_string(), data.clone()),
        ("HOME".to_string(), data.clone()),
        ("APPDATA".to_string(), data.clone()),
        ("LOCALAPPDATA".to_string(), data.clone()),
        ("TEMP".to_string(), tmp.clone()),
        ("TMP".to_string(), tmp),
        ("LIGHTAIR_PACKAGE".to_string(), spec.id.clone()),
    ];
    for k in ["NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE", "LANG"] {
        if let Ok(v) = std::env::var(k) {
            env.push((k.to_string(), v));
        }
    }
    env.extend(spec.env.iter().cloned());
    env
}

/// A path as programs expect it in their environment: `C:\…`, not the `\\?\C:\…` form
/// `canonicalize` gives.
fn plain(p: &Path) -> String {
    let s = p.to_string_lossy();
    match s.strip_prefix(r"\\?\") {
        Some(rest) if rest.as_bytes().get(1) == Some(&b':') => rest.to_string(),
        _ => s.into_owned(),
    }
}

/// `lightair-sandbox` → `lightair-sandbox.exe` when the caller left the extension off.
fn exe_path(p: &Path) -> PathBuf {
    if p.extension().is_none() { p.with_extension("exe") } else { p.to_path_buf() }
}

// ---- SIDs and the AppContainer ----

/// A SID in memory of our own.
pub(crate) struct Sid(Vec<u64>);

impl Sid {
    /// Copies a SID the system allocated (the caller frees the original).
    unsafe fn copy(src: PSID) -> io::Result<Sid> {
        unsafe {
            let len = GetLengthSid(src);
            let mut buf = vec![0u64; (len as usize).div_ceil(8)];
            if CopySid(len, buf.as_mut_ptr() as PSID, src) == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Sid(buf))
        }
    }

    fn psid(&self) -> PSID {
        self.0.as_ptr() as PSID
    }
}

impl std::fmt::Display for Sid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        unsafe {
            let mut s: *mut u16 = null_mut();
            if ConvertSidToStringSidW(self.psid(), &mut s) == 0 {
                return f.write_str("?");
            }
            let text = from_wide_ptr(s);
            LocalFree(s as _);
            f.write_str(&text)
        }
    }
}

/// The helper's AppContainer, made the first time it starts (`Lightair.Helper.<id>`).
fn container(id: &str) -> io::Result<Sid> {
    let name = container_name(id);
    let w = wide(OsStr::new(&name));
    let display = wide(OsStr::new(&format!("Lightair plugin {id}")));
    let mut sid: PSID = null_mut();
    unsafe {
        let mut hr = CreateAppContainerProfile(w.as_ptr(), display.as_ptr(), display.as_ptr(), null(), 0, &mut sid);
        // HRESULT_FROM_WIN32(ERROR_ALREADY_EXISTS)
        if hr == 0x800700B7u32 as i32 {
            hr = DeriveAppContainerSidFromAppContainerName(w.as_ptr(), &mut sid);
        }
        if hr < 0 || sid.is_null() {
            return Err(io::Error::other(format!("couldn't make the helper's AppContainer {name} ({hr:#x})")));
        }
        let owned = Sid::copy(sid);
        FreeSid(sid);
        owned
    }
}

/// An AppContainer name: at most 64 characters of letters, digits, `.` and `-`.
fn container_name(id: &str) -> String {
    let clean: String =
        id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' { c } else { '-' }).collect();
    let prefix = "Lightair.Helper.";
    if prefix.len() + clean.len() <= 64 {
        return format!("{prefix}{clean}");
    }
    // a long id: its start and a hash of all of it
    let hash = id.bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("{prefix}{}-{:08x}", &clean[..64 - prefix.len() - 9], hash as u32)
}

/// The capability SID of a capability name.
fn capability(name: &str) -> io::Result<Sid> {
    let w = wide(OsStr::new(name));
    unsafe {
        let (mut groups, mut ngroups, mut sids, mut nsids): (*mut PSID, u32, *mut PSID, u32) =
            (null_mut(), 0, null_mut(), 0);
        if DeriveCapabilitySidsFromName(w.as_ptr(), &mut groups, &mut ngroups, &mut sids, &mut nsids) == 0 {
            return Err(io::Error::last_os_error());
        }
        let out = if nsids > 0 { Sid::copy(*sids) } else { Err(io::Error::other("no capability SID")) };
        for i in 0..ngroups as usize {
            LocalFree(*groups.add(i) as _);
        }
        for i in 0..nsids as usize {
            LocalFree(*sids.add(i) as _);
        }
        LocalFree(groups as _);
        LocalFree(sids as _);
        out
    }
}

/// The AppContainer capabilities the granted holes need. Most need none: network goes through
/// the bridge, files are granted to the container itself.
fn capabilities(g: &Grants) -> io::Result<Vec<Sid>> {
    let mut names: Vec<&str> = Vec::new();
    // multicast discovery needs the private network capability, which also lets it reach the
    // home network directly (listening ports, even on the home network, are held by Lightair)
    if g.lan_discovery {
        names.push("privateNetworkClientServer");
    }
    for d in &g.devices {
        // AppContainer can't tell vendors apart either: a vendor's permission opens the class
        match d {
            Device::Hid(_) => names.push("humanInterfaceDevice"),
            Device::Usb(_) => names.push("lowLevelDevices"),
            Device::Serial => names.push("serialcommunication"),
            Device::Bluetooth => names.push("bluetooth"),
            Device::Midi => {}
        }
    }
    names.dedup();
    names.into_iter().map(capability).collect()
}

// ---- File grants ----

/// Grants the AppContainer `access` to a folder (and everything in it) or a file, unless it has
/// it already.
fn grant(path: &Path, sid: &Sid, access: u32) -> io::Result<()> {
    set_entry(path, sid, access, GRANT_ACCESS)
}

/// Grants reading of one file (the bridge's program), not its folder.
fn grant_file_read(path: &Path, sid: &Sid) -> io::Result<()> {
    set_entry(path, sid, READ, GRANT_ACCESS)
}

/// Takes the AppContainer's entries off a path.
fn revoke(path: &Path, sid: &Sid) -> io::Result<()> {
    set_entry(path, sid, 0, REVOKE_ACCESS)
}

fn set_entry(path: &Path, sid: &Sid, access: u32, mode: i32) -> io::Result<()> {
    let w = wide(path.as_os_str());
    let dir = path.is_dir();
    unsafe {
        let mut dacl: *mut ACL = null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let rc = GetNamedSecurityInfoW(
            w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        );
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        let has = has_entry(dacl, sid, access, dir);
        let skip = if mode == GRANT_ACCESS { has } else { !has_entry(dacl, sid, 0, false) };
        if skip {
            LocalFree(sd as _);
            return Ok(());
        }
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: access,
            grfAccessMode: mode,
            grfInheritance: if dir { SUB_CONTAINERS_AND_OBJECTS_INHERIT } else { NO_INHERITANCE },
            Trustee: TRUSTEE_W {
                pMultipleTrustee: null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_WELL_KNOWN_GROUP,
                ptstrName: sid.psid() as *mut u16,
            },
        };
        let mut new: *mut ACL = null_mut();
        let rc = SetEntriesInAclW(1, &ea, dacl, &mut new);
        LocalFree(sd as _);
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        let rc = SetNamedSecurityInfoW(
            w.as_ptr() as _,
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            new,
            null(),
        );
        LocalFree(new as _);
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
    }
    Ok(())
}

/// Whether the ACL has an allowing entry for `sid` with at least `access` (inherited by what's
/// inside, for a folder).
unsafe fn has_entry(acl: *mut ACL, sid: &Sid, access: u32, dir: bool) -> bool {
    if acl.is_null() {
        return false;
    }
    unsafe {
        for i in 0..(*acl).AceCount as u32 {
            let mut ace: *mut core::ffi::c_void = null_mut();
            if GetAce(acl, i, &mut ace) == 0 {
                continue;
            }
            let header = &*(ace as *const ACE_HEADER);
            if header.AceType != 0 {
                continue; // not ACCESS_ALLOWED_ACE_TYPE
            }
            let allowed = &*(ace as *const ACCESS_ALLOWED_ACE);
            let ace_sid = &allowed.SidStart as *const u32 as PSID;
            let inherits = (header.AceFlags as u32 & (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE))
                == (OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE);
            if EqualSid(ace_sid, sid.psid()) != 0 && allowed.Mask & access == access && (!dir || inherits) {
                return true;
            }
        }
    }
    false
}

/// `ipc:<name>`: lets the AppContainer open another app's named pipe `\\.\pipe\<name>`, which
/// must be running. Its owner (the user) may change its access: the AppContainer is added to it,
/// and its integrity label lowered so a low-integrity client may write to it. The pipe keeps that
/// while any instance of it is open (a server keeps one listening); a server that closes every
/// instance makes a new pipe with its own access, and the helper is refused again.
fn open_pipe(name: &str, sid: &Sid) -> io::Result<()> {
    if name.is_empty() || name.contains(['\\', '/']) || name.contains("..") {
        return Err(io::Error::other("a pipe's name, without a path"));
    }
    let w = wide(OsStr::new(&format!(r"\\.\pipe\{name}")));
    unsafe {
        // opening it takes one of its instances for a moment; WaitNamedPipe gives the server time
        // to put one up
        WaitNamedPipeW(w.as_ptr(), 2000);
        let h =
            CreateFileW(w.as_ptr(), READ_CONTROL | WRITE_DAC | WRITE_OWNER, 0, null(), OPEN_EXISTING, 0, null_mut());
        if h == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let pipe = OwnedHandle::from_raw_handle(h as _);
        let h = pipe.as_raw_handle() as HANDLE;
        let mut dacl: *mut ACL = null_mut();
        let mut sd: PSECURITY_DESCRIPTOR = null_mut();
        let rc = GetSecurityInfo(
            h,
            SE_KERNEL_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        );
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        let ea = EXPLICIT_ACCESS_W {
            grfAccessPermissions: GENERIC_READ | GENERIC_WRITE,
            grfAccessMode: GRANT_ACCESS,
            grfInheritance: NO_INHERITANCE,
            Trustee: TRUSTEE_W {
                pMultipleTrustee: null_mut(),
                MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
                TrusteeForm: TRUSTEE_IS_SID,
                TrusteeType: TRUSTEE_IS_WELL_KNOWN_GROUP,
                ptstrName: sid.psid() as *mut u16,
            },
        };
        let mut new: *mut ACL = null_mut();
        let rc = SetEntriesInAclW(1, &ea, dacl, &mut new);
        LocalFree(sd as _);
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
        // the low label: no-write-up then stops only clients below low
        let mut label_sd: PSECURITY_DESCRIPTOR = null_mut();
        let sddl = wide(OsStr::new("S:(ML;;NW;;;LW)"));
        let mut sacl: *mut ACL = null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut label_sd,
            null_mut(),
        ) != 0
        {
            let (mut present, mut defaulted) = (0, 0);
            GetSecurityDescriptorSacl(label_sd, &mut present, &mut sacl, &mut defaulted);
        }
        let mut rc =
            SetSecurityInfo(h, SE_KERNEL_OBJECT, DACL_SECURITY_INFORMATION, null_mut(), null_mut(), new, null());
        if rc == 0 && !sacl.is_null() {
            rc = SetSecurityInfo(h, SE_KERNEL_OBJECT, LABEL_SECURITY_INFORMATION, null_mut(), null_mut(), null(), sacl);
        }
        LocalFree(new as _);
        if !label_sd.is_null() {
            LocalFree(label_sd as _);
        }
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc as i32));
        }
    }
    Ok(())
}

// ---- The job object ----

pub(crate) struct Job(OwnedHandle);

impl Job {
    fn new() -> io::Result<Job> {
        unsafe {
            let h = CreateJobObjectW(null(), null());
            if h.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = Job(OwnedHandle::from_raw_handle(h as _));
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = zeroed();
            // no JOB_OBJECT_LIMIT_BREAKAWAY_OK: nothing it starts may leave the job
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_JOB_MEMORY
                | JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
            limits.JobMemoryLimit = MEMORY_CAP;
            job.set(JobObjectExtendedLimitInformation, &limits)?;
            let mut cpu: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION = zeroed();
            cpu.ControlFlags = JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP;
            cpu.Anonymous.CpuRate = CPU_CAP;
            job.set(JobObjectCpuRateControlInformation, &cpu)?;
            let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
                UIRestrictionsClass: JOB_OBJECT_UILIMIT_DESKTOP
                    | JOB_OBJECT_UILIMIT_DISPLAYSETTINGS
                    | JOB_OBJECT_UILIMIT_EXITWINDOWS
                    | JOB_OBJECT_UILIMIT_GLOBALATOMS
                    | JOB_OBJECT_UILIMIT_HANDLES
                    | JOB_OBJECT_UILIMIT_READCLIPBOARD
                    | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
                    | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
            };
            job.set(JobObjectBasicUIRestrictions, &ui)?;
            Ok(job)
        }
    }

    unsafe fn set<T>(&self, class: i32, info: &T) -> io::Result<()> {
        let ok = unsafe {
            SetInformationJobObject(
                self.0.as_raw_handle() as HANDLE,
                class,
                info as *const T as _,
                size_of::<T>() as u32,
            )
        };
        if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    fn handle(&self) -> HANDLE {
        self.0.as_raw_handle() as HANDLE
    }
}

// ---- Starting a process in the AppContainer ----

/// Starts `program` in the AppContainer `sid` with `caps`, inside `job`, with pipes for its
/// stdin, stdout and stderr, and no right to start programs of its own.
fn spawn_contained(
    program: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(String, String)],
    sid: &Sid,
    caps: &[Sid],
    job: &Job,
) -> io::Result<Child> {
    let (stdin_child, stdin_parent) = pipe()?;
    let (stdout_parent, stdout_child) = pipe()?;
    let (stderr_parent, stderr_child) = pipe()?;
    for h in [&stdin_child, &stdout_child, &stderr_child] {
        unsafe { SetHandleInformation(h.as_raw_handle() as HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAG_INHERIT) };
    }

    let mut cap_attrs: Vec<SID_AND_ATTRIBUTES> =
        caps.iter().map(|c| SID_AND_ATTRIBUTES { Sid: c.psid(), Attributes: 0x4 /* SE_GROUP_ENABLED */ }).collect();
    let security = SECURITY_CAPABILITIES {
        AppContainerSid: sid.psid(),
        Capabilities: if cap_attrs.is_empty() { null_mut() } else { cap_attrs.as_mut_ptr() },
        CapabilityCount: cap_attrs.len() as u32,
        Reserved: 0,
    };
    let handles: [HANDLE; 3] = [
        stdin_child.as_raw_handle() as HANDLE,
        stdout_child.as_raw_handle() as HANDLE,
        stderr_child.as_raw_handle() as HANDLE,
    ];
    let jobs: [HANDLE; 1] = [job.handle()];
    let child_policy: u32 = PROCESS_CREATION_CHILD_PROCESS_RESTRICTED;

    unsafe {
        let mut size = 0usize;
        InitializeProcThreadAttributeList(null_mut(), 4, 0, &mut size);
        let mut list_buf = vec![0u64; size.div_ceil(8)];
        let list = list_buf.as_mut_ptr() as LPPROC_THREAD_ATTRIBUTE_LIST;
        if InitializeProcThreadAttributeList(list, 4, 0, &mut size) == 0 {
            return Err(ctx("the attribute list")(io::Error::last_os_error()));
        }
        struct ListGuard(LPPROC_THREAD_ATTRIBUTE_LIST);
        impl Drop for ListGuard {
            fn drop(&mut self) {
                unsafe { DeleteProcThreadAttributeList(self.0) };
            }
        }
        let _guard = ListGuard(list);
        let set = |attr: u32, ptr: *const core::ffi::c_void, len: usize| -> io::Result<()> {
            if UpdateProcThreadAttribute(list, 0, attr as usize, ptr, len, null_mut(), null()) == 0 {
                Err(io::Error::other(format!("attribute {attr:#x}: {}", io::Error::last_os_error())))
            } else {
                Ok(())
            }
        };
        set(
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
            &security as *const _ as _,
            size_of::<SECURITY_CAPABILITIES>(),
        )?;
        set(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, handles.as_ptr() as _, size_of::<[HANDLE; 3]>())?;
        set(PROC_THREAD_ATTRIBUTE_JOB_LIST, jobs.as_ptr() as _, size_of::<[HANDLE; 1]>())?;
        set(PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY, &child_policy as *const u32 as _, size_of::<u32>())?;

        let mut si: STARTUPINFOEXW = zeroed();
        si.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
        si.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
        si.StartupInfo.hStdInput = handles[0];
        si.StartupInfo.hStdOutput = handles[1];
        si.StartupInfo.hStdError = handles[2];
        si.lpAttributeList = list;

        let app = wide(program.as_os_str());
        let mut cmd = command_line(program, args);
        let env_block = env_block(env);
        let cwd_w = wide(cwd.as_os_str());
        let mut pi: PROCESS_INFORMATION = zeroed();
        let ok = CreateProcessW(
            app.as_ptr(),
            cmd.as_mut_ptr(),
            null(),
            null(),
            1,
            // no console: a console program gets a console host started as its child, which the
            // child process policy refuses (it fails to start, 0xC0000142)
            EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT | DETACHED_PROCESS,
            env_block.as_ptr() as _,
            cwd_w.as_ptr(),
            &si.StartupInfo,
            &mut pi,
        );
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        CloseHandle(pi.hThread);
        drop((stdin_child, stdout_child, stderr_child));
        Ok(Child::contained(
            pi.dwProcessId as i32,
            OwnedHandle::from_raw_handle(pi.hProcess as _),
            File::from(stdin_parent),
            File::from(stdout_parent),
            File::from(stderr_parent),
        ))
    }
}

/// An anonymous pipe, (read end, write end), neither inheritable yet.
fn pipe() -> io::Result<(OwnedHandle, OwnedHandle)> {
    let (mut r, mut w): (HANDLE, HANDLE) = (null_mut(), null_mut());
    unsafe {
        if CreatePipe(&mut r, &mut w, null(), 0) == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((OwnedHandle::from_raw_handle(r as _), OwnedHandle::from_raw_handle(w as _)))
    }
}

/// A command line the C runtime splits back into `program` and `args`.
fn command_line(program: &Path, args: &[String]) -> Vec<u16> {
    let mut s = String::new();
    quote(&program.to_string_lossy(), &mut s);
    for a in args {
        s.push(' ');
        quote(a, &mut s);
    }
    wide(OsStr::new(&s))
}

fn quote(arg: &str, out: &mut String) {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"', '\n']) {
        out.push_str(arg);
        return;
    }
    out.push('"');
    let mut backslashes = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            c => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(c);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
}

/// `k=v\0k=v\0\0`, sorted by name as Windows expects.
fn env_block(env: &[(String, String)]) -> Vec<u16> {
    let mut vars: Vec<&(String, String)> = Vec::new();
    for kv in env.iter().rev() {
        if !vars.iter().any(|v| v.0.eq_ignore_ascii_case(&kv.0)) {
            vars.push(kv);
        }
    }
    vars.sort_by_key(|(k, _)| k.to_ascii_uppercase());
    let mut block: Vec<u16> = Vec::new();
    for (k, v) in vars {
        block.extend(OsStr::new(&format!("{k}={v}")).encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

// ---- The network bridge ----

/// Lightair's side of a helper's network bridge: a named pipe only its AppContainer may open.
///
/// Each pipe connection starts with one line saying what it is:
/// - `out`: the helper's outgoing connection, passed to its proxy lane;
/// - `control`: the bridge's channel for Lightair's requests (`connect <inside port> <token>`:
///   connect to the helper's listening port for the connection waiting as `<token>`);
/// - `in <token>`: the bridge connected to the helper for that waiting connection;
/// - `fail <token>`: it couldn't.
struct Bridge {
    shared: Arc<Shared>,
    pipe: String,
    /// The bridge's process (the job ends it with the helper).
    child: Option<Child>,
}

/// What the pipe's server and the listening ports share.
struct Shared {
    lane_port: Option<u16>,
    control: Mutex<Option<Arc<OverlappedPipe>>>,
    /// Connections accepted on outside ports, waiting for the bridge's `in <token>`.
    waiting: Mutex<HashMap<u64, (TcpStream, Instant)>>,
    next_token: AtomicU64,
    stop: AtomicBool,
}

impl Bridge {
    /// Opens the pipe, starts `lightair-sandbox.exe bridge <pipe>` in the AppContainer and job,
    /// and returns its port inside the container.
    fn start(
        id: &str,
        launcher: &Path,
        sid: &Sid,
        job: &Job,
        lane_port: Option<u16>,
        cwd: &Path,
    ) -> io::Result<(Bridge, u16)> {
        let pipe = format!(
            r"\\.\pipe\lightair-helper-{}-{}-{:x}",
            std::process::id(),
            container_name(id).trim_start_matches("Lightair.Helper."),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
        );
        // only the user (who owns Lightair) and this AppContainer; low label so the container
        // (low integrity) may write
        let sddl = format!("D:P(A;;GA;;;OW)(A;;GRGW;;;{sid})S:(ML;;NW;;;LW)");
        let shared = Arc::new(Shared {
            lane_port,
            control: Mutex::new(None),
            waiting: Mutex::new(HashMap::new()),
            next_token: AtomicU64::new(1),
            stop: AtomicBool::new(false),
        });
        // the first instance is made here, so a squatter can't own the name first
        let first = PipeServer::create(&pipe, &sddl, true).map_err(ctx("its pipe"))?;
        {
            let (shared, pipe, sddl) = (shared.clone(), pipe.clone(), sddl.clone());
            std::thread::Builder::new()
                .name(format!("helper bridge {id}"))
                .spawn(move || serve_pipe(first, &pipe, &sddl, &shared))?;
        }
        let mut bridge = Bridge { shared, pipe: pipe.clone(), child: None };
        let mut child = spawn_contained(launcher, &["bridge".into(), pipe.clone()], cwd, &bridge_env(), sid, &[], job)
            .inspect_err(|_| bridge.stop())
            .map_err(ctx("starting the bridge"))?;
        // it says its port on its first line
        let mut line = Vec::new();
        let mut out = child.stdout.take().ok_or_else(|| io::Error::other("the bridge has no stdout"))?;
        let mut byte = [0u8; 1];
        while line.len() < 64 {
            match out.read(&mut byte) {
                Ok(1) if byte[0] == b'\n' => break,
                Ok(1) => line.push(byte[0]),
                _ => break,
            }
        }
        let text = String::from_utf8_lossy(&line);
        let port = text.trim().strip_prefix("port ").and_then(|p| p.parse::<u16>().ok()).ok_or_else(|| {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                let _ = e.read_to_string(&mut err);
            }
            let code = child.wait();
            io::Error::other(format!("the network bridge didn't start ({code:?}): {} {}", text.trim(), err.trim()))
        })?;
        bridge.child = Some(child);
        Ok((bridge, port))
    }

    fn stop(&mut self) {
        if let Some(c) = &mut self.child {
            c.kill();
        }
        if self.shared.stop.swap(true, Ordering::Relaxed) {
            return;
        }
        // wakes the waiting ConnectNamedPipe
        let w = wide(OsStr::new(&self.pipe));
        unsafe {
            let h = CreateFileW(w.as_ptr(), GENERIC_READ, 0, null(), OPEN_EXISTING, 0, null_mut());
            if h != INVALID_HANDLE_VALUE {
                CloseHandle(h);
            }
        }
    }
}

fn bridge_env() -> Vec<(String, String)> {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
    let mut env = vec![("SystemRoot".into(), root.clone()), ("PATH".into(), format!(r"{root}\System32"))];
    // an AppContainer process can't be started without it (Windows finds the container's own
    // folder from it)
    if let Ok(v) = std::env::var("LOCALAPPDATA") {
        env.push(("LOCALAPPDATA".into(), v));
    }
    env
}

/// Accepts the bridge's pipe connections and handles each by its first line.
fn serve_pipe(first: PipeServer, name: &str, sddl: &str, shared: &Arc<Shared>) {
    let mut next = Some(first);
    while !shared.stop.load(Ordering::Relaxed) {
        let server = match next.take() {
            Some(s) => s,
            None => match PipeServer::create(name, sddl, false) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!("helper bridge pipe: {e}");
                    return;
                }
            },
        };
        if let Err(e) = server.accept() {
            tracing::warn!("helper bridge pipe: {e}");
            continue;
        }
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        let conn = Arc::new(server.0);
        let shared = shared.clone();
        let _ = std::thread::Builder::new().name("helper bridge connection".into()).spawn(move || {
            let Some(head) = read_line(&conn) else { return };
            let mut words = head.split(' ');
            match (words.next(), words.next().and_then(|t| t.parse::<u64>().ok())) {
                (Some("out"), _) => {
                    let Some(port) = shared.lane_port else { return };
                    let Ok(up) = TcpStream::connect(("127.0.0.1", port)) else { return };
                    relay(conn, up);
                }
                (Some("control"), _) => {
                    *shared.control.lock().unwrap_or_else(|e| e.into_inner()) = Some(conn);
                }
                (Some("in"), Some(token)) => {
                    let waiting = shared.waiting.lock().unwrap_or_else(|e| e.into_inner()).remove(&token);
                    if let Some((tcp, _)) = waiting {
                        relay(conn, tcp);
                    }
                }
                (Some("fail"), Some(token)) => {
                    shared.waiting.lock().unwrap_or_else(|e| e.into_inner()).remove(&token);
                }
                _ => {}
            }
        });
    }
}

/// One line (without its `\n`) from a pipe, read byte by byte so nothing after it is taken.
fn read_line(pipe: &OverlappedPipe) -> Option<String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while line.len() < 128 {
        match pipe.read(&mut byte) {
            Ok(1) if byte[0] == b'\n' => return String::from_utf8(line).ok(),
            Ok(1) => line.push(byte[0]),
            _ => return None,
        }
    }
    None
}

// ---- Listening ports (plugins.md §4.11, "Listening ports") ----

/// Environment variables, in order.
type Env = Vec<(String, String)>;

/// One port the helper may listen on: it listens on `inside`, inside its AppContainer, and
/// Lightair holds `outside` for it while it does.
#[derive(Clone, Debug)]
struct Listen {
    /// The declared port (`network.listen:<port>`), or none for `network.listen:any`.
    declared: Option<u16>,
    inside: u16,
    outside: u16,
}

/// The helper's listening ports and the environment that tells it where to listen:
/// `LIGHTAIR_LISTEN_<port>`, and `LIGHTAIR_LISTEN_ANY` with `LIGHTAIR_LISTEN_ANY_OUTSIDE`.
fn listen_ports(g: &Grants) -> io::Result<(Vec<Listen>, Env)> {
    let mut listens = Vec::new();
    let mut env = Vec::new();
    for &port in &g.listen {
        let inside = free_port()?;
        env.push((format!("LIGHTAIR_LISTEN_{port}"), inside.to_string()));
        listens.push(Listen { declared: Some(port), inside, outside: port });
    }
    if g.listen_any {
        let (inside, outside) = (free_port()?, free_port()?);
        env.push(("LIGHTAIR_LISTEN_ANY".into(), inside.to_string()));
        env.push(("LIGHTAIR_LISTEN_ANY_OUTSIDE".into(), outside.to_string()));
        listens.push(Listen { declared: None, inside, outside });
    }
    Ok((listens, env))
}

/// A loopback port free right now.
fn free_port() -> io::Result<u16> {
    Ok(std::net::TcpListener::bind(("127.0.0.1", 0))?.local_addr()?.port())
}

/// Watches whether the helper listens on its inside ports, and holds the outside ones meanwhile.
struct Listening {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Listening {
    fn start(
        helper: &str,
        listens: Vec<Listen>,
        job: HANDLE,
        shared: Arc<Shared>,
        events: Sender<Event>,
        lan: bool,
    ) -> io::Result<Listening> {
        let stop = Arc::new(AtomicBool::new(false));
        let (stop2, helper, job) = (stop.clone(), helper.to_string(), job as usize);
        let thread = std::thread::Builder::new()
            .name(format!("helper listening {helper}"))
            .spawn(move || watch(&helper, &listens, job as HANDLE, &shared, &events, lan, &stop2))?;
        Ok(Listening { stop, thread: Some(thread) })
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// An outside port Lightair holds for the helper. Dropping it closes the port.
struct Held {
    stop: Arc<AtomicBool>,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Every 250 ms: which inside ports a process of the helper's job listens on. Without
/// connecting to them (a sign-in server takes a stray connection badly), from the system's table
/// of listening sockets and their owners.
fn watch(
    helper: &str,
    listens: &[Listen],
    job: HANDLE,
    shared: &Arc<Shared>,
    events: &Sender<Event>,
    lan: bool,
    stop: &AtomicBool,
) {
    let mut held: Vec<Option<Held>> = listens.iter().map(|_| None).collect();
    // "in use" is said once each time the helper starts listening, not at every look
    let mut told: Vec<bool> = vec![false; listens.len()];
    while !stop.load(Ordering::Relaxed) && !shared.stop.load(Ordering::Relaxed) {
        let table = tcp_listeners();
        let ours = job_pids(job);
        for (i, l) in listens.iter().enumerate() {
            let listening = table.iter().any(|r| {
                r.port == l.inside && ours.contains(&r.pid) && (r.addr == [127, 0, 0, 1] || r.addr == [0, 0, 0, 0])
            });
            if !listening {
                if held[i].take().is_some() {
                    tracing::info!("helper {helper}: stopped listening; port {} closed", l.outside);
                }
                told[i] = false;
                continue;
            }
            if held[i].is_some() {
                continue;
            }
            let addr = if lan { [0, 0, 0, 0] } else { [127, 0, 0, 1] };
            match std::net::TcpListener::bind((std::net::Ipv4Addr::from(addr), l.outside)) {
                Ok(listener) => {
                    tracing::info!("helper {helper}: listening; Lightair holds port {}", l.outside);
                    held[i] = hold(helper, l, listener, shared, events).ok();
                }
                Err(_) if !told[i] => {
                    told[i] = true;
                    let owner = table
                        .iter()
                        .find(|r| r.port == l.outside && !ours.contains(&r.pid))
                        .map(|r| program_name(r.pid).unwrap_or_else(|| format!("process {}", r.pid)));
                    let _ = events.send(Event {
                        helper: helper.to_string(),
                        host: owner.unwrap_or_else(|| "another program".into()),
                        port: l.outside,
                        allowed: false,
                        reason: "port in use by another program",
                        at: SystemTime::now(),
                    });
                    tracing::warn!("helper {helper}: port {} is in use", l.outside);
                }
                Err(_) => {}
            }
        }
        // connections the bridge never took (it couldn't reach the helper)
        shared
            .waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|_, (_, at)| at.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Accepts on an outside port and passes each connection to the helper through the bridge.
fn hold(
    helper: &str,
    l: &Listen,
    listener: std::net::TcpListener,
    shared: &Arc<Shared>,
    events: &Sender<Event>,
) -> io::Result<Held> {
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let (stop2, helper, l, shared, events) =
        (stop.clone(), helper.to_string(), l.clone(), shared.clone(), events.clone());
    std::thread::Builder::new().name(format!("helper port {}", l.outside)).spawn(move || {
        while !stop2.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((conn, peer)) => {
                    let _ = conn.set_nonblocking(false);
                    let _ = events.send(Event {
                        helper: helper.clone(),
                        host: peer.ip().to_string(),
                        port: l.declared.unwrap_or(l.outside),
                        allowed: true,
                        reason: "inbound connection",
                        at: SystemTime::now(),
                    });
                    let token = shared.next_token.fetch_add(1, Ordering::Relaxed);
                    let control = shared.control.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let Some(control) = control else { continue };
                    shared.waiting.lock().unwrap_or_else(|e| e.into_inner()).insert(token, (conn, Instant::now()));
                    if control.write_all(format!("connect {} {token}\n", l.inside).as_bytes()).is_err() {
                        shared.waiting.lock().unwrap_or_else(|e| e.into_inner()).remove(&token);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(30)),
                Err(e) => {
                    tracing::warn!("helper {helper}: port {}: {e}", l.outside);
                    std::thread::sleep(Duration::from_millis(200));
                }
            }
        }
    })?;
    Ok(Held { stop })
}

/// A listening IPv4 TCP socket: its address, port and owning process.
struct TcpListenerRow {
    addr: [u8; 4],
    port: u16,
    pid: u32,
}

/// The system's listening IPv4 TCP sockets.
fn tcp_listeners() -> Vec<TcpListenerRow> {
    unsafe {
        let mut size = 0u32;
        GetExtendedTcpTable(null_mut(), &mut size, 0, 2 /* AF_INET */, TCP_TABLE_OWNER_PID_LISTENER, 0);
        for _ in 0..3 {
            let mut buf = vec![0u32; (size as usize).div_ceil(4) + 64];
            size = (buf.len() * 4) as u32;
            let rc = GetExtendedTcpTable(buf.as_mut_ptr() as _, &mut size, 0, 2, TCP_TABLE_OWNER_PID_LISTENER, 0);
            if rc == 0 {
                let table = &*(buf.as_ptr() as *const MIB_TCPTABLE_OWNER_PID);
                let rows = std::slice::from_raw_parts(table.table.as_ptr(), table.dwNumEntries as usize);
                return rows
                    .iter()
                    .map(|r| TcpListenerRow {
                        addr: r.dwLocalAddr.to_ne_bytes(),
                        port: u16::from_be(r.dwLocalPort as u16),
                        pid: r.dwOwningPid,
                    })
                    .collect();
            }
        }
        Vec::new()
    }
}

/// The ids of the processes in a job.
fn job_pids(job: HANDLE) -> Vec<u32> {
    unsafe {
        // the header (two u32s) then the ids, as pointer-sized values
        let mut buf = vec![0usize; 2 + 256];
        if QueryInformationJobObject(
            job,
            JobObjectBasicProcessIdList,
            buf.as_mut_ptr() as _,
            (buf.len() * size_of::<usize>()) as u32,
            null_mut(),
        ) == 0
        {
            return Vec::new();
        }
        let list = &*(buf.as_ptr() as *const JOBOBJECT_BASIC_PROCESS_ID_LIST);
        let n = list.NumberOfProcessIdsInList as usize;
        std::slice::from_raw_parts(list.ProcessIdList.as_ptr(), n).iter().map(|&p| p as u32).collect()
    }
}

/// A process's program file name (`Codex.exe`), when this user may ask.
fn program_name(pid: u32) -> Option<String> {
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return None;
        }
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len);
        CloseHandle(h);
        if ok == 0 {
            return None;
        }
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Some(Path::new(&path).file_name()?.to_string_lossy().into_owned())
    }
}

/// One instance of a named pipe, waiting for or holding a connection.
struct PipeServer(OverlappedPipe);

impl PipeServer {
    fn create(name: &str, sddl: &str, first: bool) -> io::Result<PipeServer> {
        let w = wide(OsStr::new(name));
        let sddl_w = wide(OsStr::new(sddl));
        unsafe {
            let mut sd: PSECURITY_DESCRIPTOR = null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl_w.as_ptr(),
                SDDL_REVISION_1,
                &mut sd,
                null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let sa = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: sd,
                bInheritHandle: 0,
            };
            let h = CreateNamedPipeW(
                w.as_ptr(),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_OVERLAPPED | if first { FILE_FLAG_FIRST_PIPE_INSTANCE } else { 0 },
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                &sa,
            );
            LocalFree(sd as _);
            if h == INVALID_HANDLE_VALUE {
                return Err(io::Error::last_os_error());
            }
            Ok(PipeServer(OverlappedPipe(OwnedHandle::from_raw_handle(h as _))))
        }
    }

    fn accept(&self) -> io::Result<()> {
        unsafe {
            let mut ov: OVERLAPPED = zeroed();
            let event = Event_::new()?;
            ov.hEvent = event.0;
            if ConnectNamedPipe(self.0.raw(), &mut ov) != 0 {
                return Ok(());
            }
            match GetLastError() {
                ERROR_PIPE_CONNECTED => Ok(()),
                ERROR_IO_PENDING => {
                    let mut n = 0;
                    if GetOverlappedResult(self.0.raw(), &ov, &mut n, 1) == 0 {
                        Err(io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                }
                e => Err(io::Error::from_raw_os_error(e as i32)),
            }
        }
    }
}

/// A pipe handle opened for overlapped I/O, so one thread may read while another writes.
pub(crate) struct OverlappedPipe(OwnedHandle);

unsafe impl Sync for OverlappedPipe {}

impl OverlappedPipe {
    fn raw(&self) -> HANDLE {
        self.0.as_raw_handle() as HANDLE
    }

    /// Opens the client end of `name` (waiting a little when every instance is busy).
    pub(crate) fn open(name: &str) -> io::Result<OverlappedPipe> {
        let w = wide(OsStr::new(name));
        for _ in 0..50 {
            unsafe {
                let h = CreateFileW(
                    w.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    0,
                    null(),
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    null_mut(),
                );
                if h != INVALID_HANDLE_VALUE {
                    return Ok(OverlappedPipe(OwnedHandle::from_raw_handle(h as _)));
                }
                if GetLastError() != ERROR_PIPE_BUSY {
                    return Err(io::Error::last_os_error());
                }
                WaitNamedPipeW(w.as_ptr(), 200);
            }
        }
        Err(io::Error::other("the pipe stayed busy"))
    }

    pub(crate) fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        self.io(buf.as_mut_ptr(), buf.len(), false)
    }

    pub(crate) fn write_all(&self, mut buf: &[u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let n = self.io(buf.as_ptr() as *mut u8, buf.len(), true)?;
            if n == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            buf = &buf[n..];
        }
        Ok(())
    }

    fn io(&self, ptr: *mut u8, len: usize, write: bool) -> io::Result<usize> {
        unsafe {
            let mut ov: OVERLAPPED = zeroed();
            let event = Event_::new()?;
            ov.hEvent = event.0;
            let mut n = 0u32;
            let len = len.min(u32::MAX as usize) as u32;
            let ok = if write {
                WriteFile(self.raw(), ptr, len, &mut n, &mut ov)
            } else {
                ReadFile(self.raw(), ptr, len, &mut n, &mut ov)
            };
            if ok == 0 {
                let e = GetLastError();
                if e != ERROR_IO_PENDING {
                    // a closed pipe reads as the end
                    return if !write && (e == 109 || e == 232) {
                        Ok(0)
                    } else {
                        Err(io::Error::from_raw_os_error(e as i32))
                    };
                }
                if GetOverlappedResult(self.raw(), &ov, &mut n, 1) == 0 {
                    let e = GetLastError();
                    return if !write && (e == 109 || e == 232) {
                        Ok(0)
                    } else {
                        Err(io::Error::from_raw_os_error(e as i32))
                    };
                }
            }
            Ok(n as usize)
        }
    }

    /// Ends any read or write in progress on it (the other direction finished).
    pub(crate) fn cancel(&self) {
        unsafe { CancelIoEx(self.raw(), null()) };
    }
}

/// A manual-reset event for one overlapped operation.
struct Event_(HANDLE);

impl Event_ {
    fn new() -> io::Result<Event_> {
        let h = unsafe { CreateEventW(null(), 1, 0, null()) };
        if h.is_null() { Err(io::Error::last_os_error()) } else { Ok(Event_(h)) }
    }
}

impl Drop for Event_ {
    fn drop(&mut self) {
        unsafe { CloseHandle(self.0) };
    }
}

/// Copies both ways between a pipe and a TCP connection until either side closes.
pub(crate) fn relay(pipe: Arc<OverlappedPipe>, tcp: TcpStream) {
    let (Ok(mut tcp_read), p2) = (tcp.try_clone(), pipe.clone()) else { return };
    let t = std::thread::spawn(move || {
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match tcp_read.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if p2.write_all(&buf[..n]).is_err() {
                        break;
                    }
                }
            }
        }
        p2.cancel();
    });
    let mut buf = vec![0u8; 32 * 1024];
    let mut tcp_write = tcp;
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tcp_write.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }
    let _ = tcp_write.shutdown(std::net::Shutdown::Both);
    let _ = t.join();
}

// ---- strings ----

fn wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

unsafe fn from_wide_ptr(p: *const u16) -> String {
    unsafe {
        let mut len = 0;
        while *p.add(len) != 0 {
            len += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, len))
    }
}

/// The bridge's side, run by `lightair-sandbox.exe bridge <pipe>` inside the helper's
/// AppContainer: opens its control channel to Lightair, listens on 127.0.0.1, says `port <n>` on
/// stdout, and carries each connection to Lightair over `pipe`. When Lightair asks, it connects to
/// the helper's own listening port for a connection Lightair accepted outside. It ends when
/// Lightair closes the control channel.
pub fn bridge_main(pipe: &str) -> io::Result<()> {
    let control = OverlappedPipe::open(pipe)?;
    control.write_all(b"control\n")?;
    {
        let pipe = pipe.to_string();
        std::thread::spawn(move || {
            while let Some(line) = read_line(&control) {
                let mut w = line.split(' ');
                let (Some("connect"), Some(port), Some(token)) = (w.next(), w.next(), w.next()) else { continue };
                let (Ok(port), token, pipe) = (port.parse::<u16>(), token.to_string(), pipe.clone()) else { continue };
                std::thread::spawn(move || {
                    let Ok(p) = OverlappedPipe::open(&pipe) else { return };
                    match TcpStream::connect(("127.0.0.1", port)) {
                        Ok(tcp) => {
                            if p.write_all(format!("in {token}\n").as_bytes()).is_ok() {
                                relay(Arc::new(p), tcp);
                            }
                        }
                        Err(e) => {
                            eprintln!("lightair-sandbox bridge: the helper's port {port}: {e}");
                            let _ = p.write_all(format!("fail {token}\n").as_bytes());
                        }
                    }
                });
            }
            std::process::exit(0);
        });
    }
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    {
        let mut out = io::stdout().lock();
        writeln!(out, "port {port}")?;
        out.flush()?;
    }
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let pipe = pipe.to_string();
        std::thread::spawn(move || match OverlappedPipe::open(&pipe) {
            Ok(p) => {
                if p.write_all(b"out\n").is_ok() {
                    relay(Arc::new(p), conn);
                }
            }
            Err(e) => eprintln!("lightair-sandbox bridge: {e}"),
        });
    }
    Ok(())
}
