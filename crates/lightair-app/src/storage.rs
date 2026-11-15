//! The recording drive's free space, watched while recording: sampled every second (every ten
//! when nothing records), with how fast it's going down and how long that leaves.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How far back the rate looks: long enough to smooth a recorder's bursty writes.
const WINDOW: Duration = Duration::from_secs(30);
/// The rate needs this much history before it's shown.
const SETTLE: Duration = Duration::from_secs(5);

#[derive(Default)]
pub struct DiskWatch {
    folder: Option<PathBuf>,
    /// (free, total) bytes at the last look.
    pub space: Option<(u64, u64)>,
    checked: Option<Instant>,
    /// Free bytes over the last [`WINDOW`], while recording.
    samples: VecDeque<(Instant, u64)>,
}

impl DiskWatch {
    /// Looks again when it's time (`recording`: every second; otherwise every ten, and the
    /// rate starts over); `read` gives (free, total) for a folder.
    pub fn poll(&mut self, folder: &Path, recording: bool, now: Instant, read: impl Fn(&Path) -> Option<(u64, u64)>) {
        let moved = self.folder.as_deref() != Some(folder);
        let every = if recording { Duration::from_secs(1) } else { Duration::from_secs(10) };
        if !moved && self.checked.is_some_and(|t| now.duration_since(t) < every) {
            return;
        }
        if moved || !recording {
            self.samples.clear();
        }
        self.folder = Some(folder.to_path_buf());
        self.checked = Some(now);
        self.space = read(folder);
        if let (true, Some((free, _))) = (recording, self.space) {
            self.samples.push_back((now, free));
            while self.samples.front().is_some_and(|(t, _)| now.duration_since(*t) > WINDOW) {
                self.samples.pop_front();
            }
        }
    }

    /// Bytes a second the free space is going down, once there's enough history (`None` while
    /// it isn't going down).
    pub fn rate(&self) -> Option<f64> {
        let ((t0, f0), (t1, f1)) = (self.samples.front()?, self.samples.back()?);
        let dt = t1.duration_since(*t0);
        if dt < SETTLE || f1 >= f0 {
            return None;
        }
        Some((f0 - f1) as f64 / dt.as_secs_f64())
    }

    /// How long the free space lasts at the current rate.
    pub fn time_left(&self) -> Option<Duration> {
        let rate = self.rate()?;
        let (free, _) = self.space?;
        Some(Duration::from_secs_f64(free as f64 / rate))
    }
}

/// "214.32 GB", "812.4 MB" (decimal units, as Finder and Explorer show); `fine`: one more
/// decimal, so the number visibly moves while recording.
pub fn bytes(b: u64, fine: bool) -> String {
    let (value, unit) = match b {
        b if b >= 1_000_000_000_000 => (b as f64 / 1e12, "TB"),
        b if b >= 1_000_000_000 => (b as f64 / 1e9, "GB"),
        b if b >= 1_000_000 => (b as f64 / 1e6, "MB"),
        b => (b as f64 / 1e3, "KB"),
    };
    let decimals = match (unit, fine) {
        ("TB", true) => 3,
        (_, true) => 2,
        (_, false) => 1,
    };
    format!("{value:.decimals$} {unit}")
}

/// "~13 h left", "~42 min left", "under a minute left".
pub fn left(d: Duration) -> String {
    let mins = d.as_secs() / 60;
    match mins {
        0 => "under a minute left".into(),
        m if m < 60 => format!("~{m} min left"),
        m if m < 48 * 60 => format!("~{} h {:02} min left", m / 60, m % 60),
        m => format!("~{} days left", m / (24 * 60)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn rate_and_time_left_follow_the_drop_while_recording() {
        let mut w = DiskWatch::default();
        let free = Cell::new(10_000_000_000u64);
        let start = Instant::now();
        let folder = Path::new("/recordings");
        // 10 MB a second for 10 s
        for s in 0..=10 {
            w.poll(folder, true, start + Duration::from_secs(s), |_| Some((free.get(), 500_000_000_000)));
            free.set(free.get() - 10_000_000);
        }
        let rate = w.rate().unwrap();
        assert!((rate - 10_000_000.0).abs() < 1.0, "{rate}");
        let left = w.time_left().unwrap().as_secs();
        assert_eq!(left, (10_000_000_000 - 10 * 10_000_000) / 10_000_000);
        // stopped: the next look (ten seconds on) starts the rate over
        w.poll(folder, false, start + Duration::from_secs(25), |_| Some((free.get(), 500_000_000_000)));
        assert!(w.rate().is_none());
    }

    #[test]
    fn no_rate_until_it_settles_or_while_space_grows() {
        let mut w = DiskWatch::default();
        let start = Instant::now();
        let folder = Path::new("/recordings");
        w.poll(folder, true, start, |_| Some((1_000, 2_000)));
        w.poll(folder, true, start + Duration::from_secs(2), |_| Some((900, 2_000)));
        assert!(w.rate().is_none(), "too little history");
        w.poll(folder, true, start + Duration::from_secs(8), |_| Some((1_500, 2_000)));
        assert!(w.rate().is_none(), "space came back (files deleted)");
    }

    #[test]
    fn sizes_and_durations_read_naturally() {
        assert_eq!(bytes(214_318_000_000, false), "214.3 GB");
        assert_eq!(bytes(214_318_000_000, true), "214.32 GB");
        assert_eq!(bytes(812_400_000, false), "812.4 MB");
        assert_eq!(left(Duration::from_secs(42 * 60 + 5)), "~42 min left");
        assert_eq!(left(Duration::from_secs(13 * 3600 + 7 * 60)), "~13 h 07 min left");
        assert_eq!(left(Duration::from_secs(30)), "under a minute left");
    }
}
