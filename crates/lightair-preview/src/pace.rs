//! Keeps a preview's program frames to the rate it asks for (`Preview::set_rate`): the
//! compositor draws the program at the canvas's rate for the outputs anyway, and a preview that
//! asks for fewer frames shows only some of them. Showing fewer is what saves the work (the copy
//! into the window, and on a laptop with two graphics adapters, the trip to the one driving the
//! display).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

/// When the next frame is due.
#[derive(Default)]
pub(crate) struct Pace {
    /// Nanoseconds since [`epoch`]; 0: the next frame is shown.
    next: AtomicU64,
}

fn epoch() -> Instant {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

impl Pace {
    /// Whether a frame arriving now is shown at `fps` (0: every frame is).
    pub(crate) fn due(&self, fps: u32) -> bool {
        self.due_at(fps, epoch().elapsed().as_nanos() as u64)
    }

    /// [`Pace::due`] for a frame arriving `now` ns after the epoch.
    fn due_at(&self, fps: u32, now: u64) -> bool {
        if fps == 0 {
            self.next.store(0, Ordering::Relaxed);
            return true;
        }
        let period = 1_000_000_000 / fps as u64;
        let next = self.next.load(Ordering::Relaxed);
        // a quarter period early still counts: frames arrive on the canvas's beat, not this one's
        if now + period / 4 < next {
            return false;
        }
        // keep the beat, unless it fell behind (a pause, a slow frame)
        let after = if next == 0 || now > next + period { now + period } else { next + period };
        self.next.store(after, Ordering::Relaxed);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How many of `frames` frames arriving at `canvas` fps are shown at `fps`.
    fn shown(canvas: u64, fps: u32, frames: u64) -> u64 {
        let pace = Pace::default();
        let tick = 1_000_000_000 / canvas;
        (1..=frames).filter(|i| pace.due_at(fps, i * tick)).count() as u64
    }

    #[test]
    fn keeps_to_the_rate_asked() {
        assert_eq!(shown(60, 0, 600), 600, "no rate: every frame");
        assert_eq!(shown(60, 60, 600), 600);
        assert_eq!(shown(60, 30, 600), 300);
        assert_eq!(shown(60, 20, 600), 200);
        assert_eq!(shown(30, 30, 300), 300);
        assert!((440..=460).contains(&shown(60, 45, 600)), "{}", shown(60, 45, 600));
        assert_eq!(shown(30, 60, 300), 300, "never more than arrive");
    }

    #[test]
    fn starts_again_after_a_pause() {
        let pace = Pace::default();
        assert!(pace.due_at(30, 1_000));
        assert!(!pace.due_at(30, 2_000));
        assert!(pace.due_at(30, 10_000_000_000), "after a pause, at once");
        assert!(!pace.due_at(30, 10_010_000_000));
        assert!(pace.due_at(30, 10_033_400_000));
    }
}
