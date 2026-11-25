//! Lightair's clock: the host's monotonic clock in nanoseconds (`mach_absolute_time` on macOS,
//! the performance counter on Windows, `CLOCK_MONOTONIC` elsewhere), which captures, decoders,
//! the mixer and the encoders all stamp with. It's the clock libobs's `os_gettime_ns` read too,
//! so times from either agree while both are in use.

/// Nanoseconds on the host clock.
pub fn now_ns() -> u64 {
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn mach_absolute_time() -> u64;
            fn mach_timebase_info(info: *mut [u32; 2]) -> i32;
        }
        static BASE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
        let (n, d) = *BASE.get_or_init(|| {
            let mut i = [0u32; 2];
            // SAFETY: `i` is a valid place for the timebase.
            unsafe { mach_timebase_info(&mut i) };
            (i[0].max(1), i[1].max(1))
        });
        // SAFETY: no arguments; always safe to call.
        (unsafe { mach_absolute_time() } as u128 * n as u128 / d as u128) as u64
    }
    #[cfg(windows)]
    {
        unsafe extern "system" {
            fn QueryPerformanceCounter(count: *mut i64) -> i32;
            fn QueryPerformanceFrequency(frequency: *mut i64) -> i32;
        }
        static FREQUENCY: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
        let frequency = *FREQUENCY.get_or_init(|| {
            let mut f = 0;
            // SAFETY: `f` is a valid place for the frequency.
            unsafe { QueryPerformanceFrequency(&mut f) };
            f.max(1)
        });
        let mut count = 0;
        // SAFETY: `count` is a valid place for the counter.
        unsafe { QueryPerformanceCounter(&mut count) };
        (count.max(0) as i128 * 1_000_000_000 / frequency as i128) as u64
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        static START: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        START.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64
    }
}
