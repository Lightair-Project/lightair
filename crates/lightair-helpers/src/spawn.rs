//! Starting a helper's process with pipes for stdin, stdout and stderr. On macOS through
//! `posix_spawn` with responsibility disclaimed, so the process is its own responsible process
//! for privacy permissions (the helper-sandbox spike showed it survives the launcher's `exec`).

use std::fs::File;
use std::io;
use std::path::Path;

/// A running helper process.
pub struct Child {
    pid: i32,
    pub stdin: Option<File>,
    pub stdout: Option<File>,
    pub stderr: Option<File>,
    #[cfg(not(target_os = "macos"))]
    inner: Option<std::process::Child>,
    /// A process started in a sandbox (Windows), not through `std::process`.
    #[cfg(windows)]
    process: Option<std::os::windows::io::OwnedHandle>,
    done: bool,
}

impl Child {
    pub fn pid(&self) -> i32 {
        self.pid
    }

    #[cfg(windows)]
    pub(crate) fn contained(
        pid: i32,
        process: std::os::windows::io::OwnedHandle,
        stdin: File,
        stdout: File,
        stderr: File,
    ) -> Child {
        Child {
            pid,
            stdin: Some(stdin),
            stdout: Some(stdout),
            stderr: Some(stderr),
            inner: None,
            process: Some(process),
            done: false,
        }
    }

    /// Windows: the exit code of a process started in a sandbox, waiting up to `ms`.
    #[cfg(windows)]
    fn contained_exit(&mut self, ms: u32) -> Option<Option<i32>> {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
        let h = self.process.as_ref()?.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
        unsafe {
            if WaitForSingleObject(h, ms) != WAIT_OBJECT_0 {
                return Some(None);
            }
            let mut code = 0u32;
            GetExitCodeProcess(h, &mut code);
            self.done = true;
            Some(Some(code as i32))
        }
    }

    /// Ends the process (it's asked to stop by closing stdin first; this is the hard stop).
    pub fn kill(&mut self) {
        if self.done {
            return;
        }
        #[cfg(target_os = "macos")]
        unsafe {
            libc::kill(self.pid, libc::SIGKILL);
        }
        #[cfg(windows)]
        if let Some(p) = &self.process {
            use std::os::windows::io::AsRawHandle;
            unsafe { windows_sys::Win32::System::Threading::TerminateProcess(p.as_raw_handle() as _, 1) };
        }
        #[cfg(not(target_os = "macos"))]
        if let Some(c) = &mut self.inner {
            let _ = c.kill();
        }
        let _ = self.wait();
    }

    /// Its exit code if it has ended (`Some(-1)` when killed by a signal).
    pub fn try_wait(&mut self) -> Option<i32> {
        if self.done {
            return Some(-1);
        }
        #[cfg(target_os = "macos")]
        {
            let mut status = 0;
            let r = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
            if r == self.pid {
                self.done = true;
                return Some(if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { -1 });
            }
            None
        }
        #[cfg(windows)]
        if let Some(r) = self.contained_exit(0) {
            return r;
        }
        #[cfg(not(target_os = "macos"))]
        {
            let code = self.inner.as_mut()?.try_wait().ok()??.code().unwrap_or(-1);
            self.done = true;
            Some(code)
        }
    }

    /// Waits for it to end.
    pub fn wait(&mut self) -> Option<i32> {
        if self.done {
            return Some(-1);
        }
        #[cfg(target_os = "macos")]
        {
            let mut status = 0;
            let r = unsafe { libc::waitpid(self.pid, &mut status, 0) };
            self.done = true;
            (r == self.pid).then(|| if libc::WIFEXITED(status) { libc::WEXITSTATUS(status) } else { -1 })
        }
        #[cfg(windows)]
        if let Some(r) = self.contained_exit(windows_sys::Win32::System::Threading::INFINITE) {
            return r;
        }
        #[cfg(not(target_os = "macos"))]
        {
            let code = self.inner.as_mut()?.wait().ok()?.code().unwrap_or(-1);
            self.done = true;
            Some(code)
        }
    }
}

#[cfg(target_os = "macos")]
pub fn spawn(program: &Path, args: &[String], cwd: &Path, env: &[(String, String)]) -> io::Result<Child> {
    use std::ffi::{CString, c_char, c_int};
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn responsibility_spawnattrs_setdisclaim(attrs: *mut libc::posix_spawnattr_t, disclaim: c_int) -> c_int;
        fn posix_spawn_file_actions_addchdir_np(
            actions: *mut libc::posix_spawn_file_actions_t,
            path: *const c_char,
        ) -> c_int;
    }

    let cstr = |b: &[u8]| CString::new(b).map_err(|_| io::Error::other("a NUL byte in an argument"));
    let prog = cstr(program.as_os_str().as_bytes())?;
    let mut argv_owned = vec![prog.clone()];
    for a in args {
        argv_owned.push(cstr(a.as_bytes())?);
    }
    let mut argv: Vec<*mut c_char> = argv_owned.iter().map(|s| s.as_ptr() as *mut c_char).collect();
    argv.push(std::ptr::null_mut());
    let env_owned: Vec<CString> =
        env.iter().map(|(k, v)| cstr(format!("{k}={v}").as_bytes())).collect::<io::Result<_>>()?;
    let mut envp: Vec<*mut c_char> = env_owned.iter().map(|s| s.as_ptr() as *mut c_char).collect();
    envp.push(std::ptr::null_mut());
    let cwd_c = cstr(cwd.as_os_str().as_bytes())?;

    let pipe = || -> io::Result<[c_int; 2]> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        for fd in fds {
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
        }
        Ok(fds)
    };
    let (inp, out, err) = (pipe()?, pipe()?, pipe()?);

    unsafe {
        let mut attrs: libc::posix_spawnattr_t = std::ptr::null_mut();
        let mut actions: libc::posix_spawn_file_actions_t = std::ptr::null_mut();
        libc::posix_spawnattr_init(&mut attrs);
        libc::posix_spawn_file_actions_init(&mut actions);
        let close_all = |fds: &[c_int]| {
            for &fd in fds {
                libc::close(fd);
            }
        };
        let cleanup = |attrs: &mut libc::posix_spawnattr_t, actions: &mut libc::posix_spawn_file_actions_t| {
            libc::posix_spawnattr_destroy(attrs);
            libc::posix_spawn_file_actions_destroy(actions);
        };
        // Only the three pipes reach the child; every other descriptor of Lightair's stays behind.
        libc::posix_spawnattr_setflags(
            &mut attrs,
            (libc::POSIX_SPAWN_CLOEXEC_DEFAULT | libc::POSIX_SPAWN_SETSIGDEF | libc::POSIX_SPAWN_SETSIGMASK) as i16,
        );
        let mut all: libc::sigset_t = std::mem::zeroed();
        libc::sigfillset(&mut all);
        libc::posix_spawnattr_setsigdefault(&mut attrs, &all);
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        libc::posix_spawnattr_setsigmask(&mut attrs, &none);
        if responsibility_spawnattrs_setdisclaim(&mut attrs, 1) != 0 {
            cleanup(&mut attrs, &mut actions);
            close_all(&[inp[0], inp[1], out[0], out[1], err[0], err[1]]);
            return Err(io::Error::other("couldn't disclaim responsibility for the helper"));
        }
        libc::posix_spawn_file_actions_adddup2(&mut actions, inp[0], 0);
        libc::posix_spawn_file_actions_adddup2(&mut actions, out[1], 1);
        libc::posix_spawn_file_actions_adddup2(&mut actions, err[1], 2);
        posix_spawn_file_actions_addchdir_np(&mut actions, cwd_c.as_ptr());

        let mut pid: libc::pid_t = 0;
        let rc = libc::posix_spawn(&mut pid, prog.as_ptr(), &actions, &attrs, argv.as_ptr(), envp.as_ptr());
        cleanup(&mut attrs, &mut actions);
        close_all(&[inp[0], out[1], err[1]]);
        if rc != 0 {
            close_all(&[inp[1], out[0], err[0]]);
            return Err(io::Error::from_raw_os_error(rc));
        }
        Ok(Child {
            pid,
            stdin: Some(File::from_raw_fd(inp[1])),
            stdout: Some(File::from_raw_fd(out[0])),
            stderr: Some(File::from_raw_fd(err[0])),
            done: false,
        })
    }
}

#[cfg(not(target_os = "macos"))]
pub fn spawn(program: &Path, args: &[String], cwd: &Path, env: &[(String, String)]) -> io::Result<Child> {
    use std::process::{Command, Stdio};
    let mut c = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().map(|(k, v)| (k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let to_file = |h: Option<std::process::ChildStdin>| h.map(file_from_stdin);
    let pid = c.id() as i32;
    let stdin = to_file(c.stdin.take());
    let stdout = c.stdout.take().map(file_from_stdout);
    let stderr = c.stderr.take().map(file_from_stderr);
    Ok(Child {
        pid,
        stdin,
        stdout,
        stderr,
        inner: Some(c),
        #[cfg(windows)]
        process: None,
        done: false,
    })
}

#[cfg(windows)]
fn file_from_stdin(h: std::process::ChildStdin) -> File {
    File::from(std::os::windows::io::OwnedHandle::from(h))
}
#[cfg(windows)]
fn file_from_stdout(h: std::process::ChildStdout) -> File {
    File::from(std::os::windows::io::OwnedHandle::from(h))
}
#[cfg(windows)]
fn file_from_stderr(h: std::process::ChildStderr) -> File {
    File::from(std::os::windows::io::OwnedHandle::from(h))
}
#[cfg(all(unix, not(target_os = "macos")))]
fn file_from_stdin(h: std::process::ChildStdin) -> File {
    File::from(std::os::fd::OwnedFd::from(h))
}
#[cfg(all(unix, not(target_os = "macos")))]
fn file_from_stdout(h: std::process::ChildStdout) -> File {
    File::from(std::os::fd::OwnedFd::from(h))
}
#[cfg(all(unix, not(target_os = "macos")))]
fn file_from_stderr(h: std::process::ChildStderr) -> File {
    File::from(std::os::fd::OwnedFd::from(h))
}
