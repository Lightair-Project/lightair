//! Lightair's clock: the host's monotonic clock in nanoseconds (`mach_absolute_time` on macOS,
//! the performance counter on Windows), which captures, decoders, the mixer and the encoders all
//! stamp with. It's the same clock libobs's `os_gettime_ns` reads, so times from either agree.

/// Nanoseconds on the host clock.
pub fn now_ns() -> u64 {
    lightair_engine::clock::host_now_ns().max(0) as u64
}
