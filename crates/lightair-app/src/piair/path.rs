//! Natural pointer paths (piair/docs/design.md §6.2): minimum-jerk timing, a slight arc, a
//! duration from Fitts' law, a small overshoot on long moves. Steps at 120 Hz.

use std::time::Duration;

/// Points from `from` to `to` (display points) and the time between them.
pub fn path(from: (f32, f32), to: (f32, f32), ms: Option<u32>, seed: u64) -> (Vec<(f32, f32)>, Duration) {
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let dist = (dx * dx + dy * dy).sqrt();
    if dist < 1.0 {
        return (vec![to], Duration::from_millis(8));
    }
    // Fitts: a + b·log2(d/w + 1), target ~ 24 points wide
    let fitts = 180.0 + 110.0 * (dist / 24.0 + 1.0).log2();
    let total = ms.map_or(fitts.clamp(250.0, 900.0), |m| m.clamp(60, 4000) as f32);
    let step = Duration::from_micros(8333);
    let n = ((total / 8.333) as usize).max(2);
    let mut rng = seed | 1;
    let mut rand = || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng >> 40) as f32 / (1u64 << 24) as f32
    };
    // the arc: a sideways bulge of 2–6% of the distance, either side
    let side = if rand() < 0.5 { -1.0 } else { 1.0 };
    let bulge = dist * (0.02 + rand() * 0.04) * side;
    let (nx, ny) = (-dy / dist, dx / dist);
    // long moves overshoot a little and settle back
    let over = if dist > 600.0 { 0.012 + rand() * 0.012 } else { 0.0 };
    let mut pts = Vec::with_capacity(n + 8);
    for i in 1..=n {
        let t = i as f32 / n as f32;
        let s = 10.0 * t.powi(3) - 15.0 * t.powi(4) + 6.0 * t.powi(5);
        let s = s * (1.0 + over);
        let arc = (std::f32::consts::PI * t).sin() * bulge;
        pts.push((from.0 + dx * s + nx * arc, from.1 + dy * s + ny * arc));
    }
    if over > 0.0 {
        let last = *pts.last().unwrap();
        for i in 1..=8 {
            let t = i as f32 / 8.0;
            let e = 1.0 - (1.0 - t).powi(3);
            pts.push((last.0 + (to.0 - last.0) * e, last.1 + (to.1 - last.1) * e));
        }
    }
    *pts.last_mut().unwrap() = to;
    (pts, step)
}
