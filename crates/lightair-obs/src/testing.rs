//! For tests: libobs is process-global, so a test that starts an [`Engine`](crate::Engine) runs
//! in a process of its own. Each crate's tests are one test program (one link, not one per
//! file); a test that needs a process of its own starts with
//!
//! ```ignore
//! if lightair_obs::testing::ran_in_own_process() {
//!     return;
//! }
//! ```
//!
//! Under `cargo test` that re-runs the program for just that test (a few at a time) and reports
//! its result; `cargo xtask test` runs every such test as a program of its own directly.

use std::process::Command;
use std::sync::{Condvar, Mutex};

/// Set in a test's own process: the test runs there.
pub const OWN_PROCESS: &str = "LIGHTAIR_TEST_OWN_PROCESS";

static RUNNING: Mutex<usize> = Mutex::new(0);
static FREED: Condvar = Condvar::new();

/// Runs the calling test in a process of its own unless this is that process. True when the
/// test ran elsewhere (and passed): the caller returns.
pub fn ran_in_own_process() -> bool {
    if std::env::var_os(OWN_PROCESS).is_some() {
        return false;
    }
    // libtest runs each test on a thread named after it
    let name = std::thread::current().name().filter(|n| *n != "main").map(str::to_owned);
    let Some(name) = name else {
        // not under libtest's threads (e.g. --test-threads=1 on some toolchains): run here
        return false;
    };
    // half the cores: each process runs libobs and real-time audio threads of its own
    let most = std::thread::available_parallelism().map_or(4, |n| n.get().div_ceil(2).max(2));
    {
        let mut running = RUNNING.lock().unwrap();
        while *running >= most {
            running = FREED.wait(running).unwrap();
        }
        *running += 1;
    }
    let out = Command::new(std::env::current_exe().expect("this test program"))
        .args([name.as_str(), "--exact", "--include-ignored", "--nocapture", "--test-threads=1"])
        .env(OWN_PROCESS, "1")
        .output();
    *RUNNING.lock().unwrap() -= 1;
    FREED.notify_one();
    let out = out.expect("the test program runs again");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // captured by the test harness: shown when the test fails (or with --nocapture)
    print!("{stdout}");
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "{name} failed in its own process ({})", out.status);
    assert!(stdout.contains("test result: ok. 1 passed"), "{name} didn't run in its own process");
    true
}
