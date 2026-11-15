//! The recording drive's free space: read every second while recording (every ten when
//! nothing records), so it visibly goes down.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct DiskWatch {
    folder: Option<PathBuf>,
    /// (free, total) bytes at the last look.
    pub space: Option<(u64, u64)>,
    checked: Option<Instant>,
}

impl DiskWatch {
    /// Looks again when it's time (`recording`: every second; otherwise every ten, or at once
    /// for another folder); `read` gives (free, total) for a folder.
    pub fn poll(&mut self, folder: &Path, recording: bool, now: Instant, read: impl Fn(&Path) -> Option<(u64, u64)>) {
        let moved = self.folder.as_deref() != Some(folder);
        let every = if recording { Duration::from_secs(1) } else { Duration::from_secs(10) };
        if !moved && self.checked.is_some_and(|t| now.duration_since(t) < every) {
            return;
        }
        self.folder = Some(folder.to_path_buf());
        self.checked = Some(now);
        self.space = read(folder);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn looks_every_second_while_recording_every_ten_otherwise() {
        let mut w = DiskWatch::default();
        let reads = Cell::new(0);
        let read = |_: &Path| {
            reads.set(reads.get() + 1);
            Some((1_000, 2_000))
        };
        let (start, folder) = (Instant::now(), Path::new("/recordings"));
        for ms in (0..=3_000).step_by(250) {
            w.poll(folder, true, start + Duration::from_millis(ms), read);
        }
        assert_eq!(reads.get(), 4, "at 0, 1, 2 and 3 s");
        w.poll(folder, false, start + Duration::from_secs(5), read);
        assert_eq!(reads.get(), 4, "idle: not yet");
        w.poll(Path::new("/elsewhere"), false, start + Duration::from_secs(5), read);
        assert_eq!(reads.get(), 5, "another folder: at once");
        assert_eq!(w.space, Some((1_000, 2_000)));
    }

    #[test]
    fn sizes_read_naturally() {
        assert_eq!(bytes(214_318_000_000, false), "214.3 GB");
        assert_eq!(bytes(214_318_000_000, true), "214.32 GB");
        assert_eq!(bytes(812_400_000, false), "812.4 MB");
    }
}
