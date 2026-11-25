//! Lightair's own compositor (`LIGHTAIR_ENGINE=own`) against real libobs: the picture it draws
//! from the sources libobs draws, and that picture going back into libobs. macOS only.
//!
//! A program of its own, apart from `preview`: the switch is read once per process, and these
//! tests turn it on. Each test that starts libobs runs in a process of its own
//! (`lightair_obs::testing`), which sets the switch before anything reads it.

#[cfg(target_os = "macos")]
mod audio;
#[cfg(target_os = "macos")]
mod outputs;
#[cfg(target_os = "macos")]
mod overlays;
#[cfg(target_os = "macos")]
mod program;
#[cfg(target_os = "macos")]
mod record;
#[cfg(target_os = "macos")]
mod switch;

#[cfg(target_os = "macos")]
mod common {
    use std::path::Path;
    use std::time::{Duration, Instant};

    use lightair_obs::Engine;
    use lightair_model::{Command, Document, diff};
    use lightair_preview::{Preview, frame_rgba, frame_size};

    pub const W: u32 = 320;
    pub const H: u32 = 180;
    /// The size the program's preview is asked to draw at: libobs draws it at this size, the
    /// compositor at the canvas's, so a frame's size says who drew it.
    pub const SMALL: (u32, u32) = (160, 90);

    /// Turns Lightair's own compositor on for this process. Called first, before libobs starts
    /// (nothing has read the switch yet; this process runs one test, on one thread).
    pub fn switch_on() {
        // SAFETY: no other thread reads or writes the environment now (libobs isn't running).
        unsafe { std::env::set_var("LIGHTAIR_ENGINE", "own") };
        assert!(lightair_obs::bridge::own(), "the switch is on");
    }

    pub fn apply(engine: &Engine, doc: &mut Document, cmd: Command) {
        let change = doc.apply(&cmd).unwrap();
        let patches = diff(&change.before, &change.after);
        let workspace = change.after.clone();
        engine.call(move |s| s.reconciler.apply(&workspace, &patches));
    }

    /// An RGBA frame at the canvas's size.
    pub type Rgba = Vec<u8>;

    pub fn rgb(img: &[u8], fx: f32, fy: f32) -> [u8; 3] {
        let (x, y) = ((W as f32 * fx) as u32, (H as f32 * fy) as u32);
        let i = ((y * W + x) * 4) as usize;
        [img[i], img[i + 1], img[i + 2]]
    }

    /// The newest frame `preview` has had since this was last asked, at `size` (one at another
    /// size isn't returned).
    fn next_frame(preview: &Preview, seen: &mut u64, size: (u32, u32)) -> Option<Rgba> {
        let n = preview.frames();
        if n == *seen {
            return None;
        }
        *seen = n;
        let frame = preview.latest_frame()?;
        let want = (size.0 as usize, size.1 as usize);
        if frame_size(&frame) != want {
            return None;
        }
        let (w, h, rgba) = frame_rgba(&frame, want.0)?;
        ((w, h) == want).then_some(rgba)
    }

    /// Waits (up to `within`) for a frame at the canvas's size that `ok` accepts, then gives
    /// the one a few frames later (every picture in it by then). `Err` carries the last frame
    /// seen at that size, if any.
    pub fn until(preview: &Preview, within: Duration, ok: impl Fn(&[u8]) -> bool) -> Result<Rgba, Option<Rgba>> {
        until_at(preview, (W, H), within, ok)
    }

    /// [`until`] for a canvas of another size: frames at `size` only.
    pub fn until_at(
        preview: &Preview,
        size: (u32, u32),
        within: Duration,
        ok: impl Fn(&[u8]) -> bool,
    ) -> Result<Rgba, Option<Rgba>> {
        let end = Instant::now() + within;
        let mut seen = preview.frames();
        let (mut last, mut good) = (None, 0);
        while Instant::now() < end {
            if let Some(f) = next_frame(preview, &mut seen, size) {
                good = if ok(&f) { good + 1 } else { 0 };
                last = Some(f);
                if good >= 3 {
                    return Ok(last.unwrap());
                }
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        Err(last)
    }

    /// Every frame at the canvas's size `preview` gets for `long`.
    pub fn watch(preview: &Preview, long: Duration) -> Vec<Rgba> {
        let end = Instant::now() + long;
        let (mut seen, mut out) = (preview.frames(), Vec::new());
        while Instant::now() < end {
            if let Some(f) = next_frame(preview, &mut seen, (W, H)) {
                out.push(f);
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        out
    }

    /// A sampled place two frames disagree on: (x, y, one's colour, the other's).
    pub type Mismatch = (u32, u32, [u8; 3], [u8; 3]);

    pub fn save(dir: &Path, name: &str, px: &[u8]) {
        let _ = image::save_buffer(dir.join(name), px, W, H, image::ColorType::Rgba8);
    }

    /// How many sampled places differ, of how many (as `preview`'s golden_scene compares).
    /// One frame may have come through NV12 (colour at half resolution), so brightness is
    /// compared per pixel and colour per 2×2 block, and a place matches when the other frame
    /// has it within a pixel (or block) of there.
    pub fn differ(a: &[u8], b: &[u8]) -> (usize, usize, Vec<Mismatch>) {
        let at = |img: &[u8], x: u32, y: u32| {
            let i = ((y * W + x) * 4) as usize;
            [img[i] as f32, img[i + 1] as f32, img[i + 2] as f32]
        };
        let luma = |img: &[u8], x: u32, y: u32| {
            let [r, g, b] = at(img, x, y);
            0.299 * r + 0.587 * g + 0.114 * b
        };
        let chroma = |img: &[u8], bx: u32, by: u32| {
            let (mut cb, mut cr) = (0.0, 0.0);
            for (x, y) in [(bx * 2, by * 2), (bx * 2 + 1, by * 2), (bx * 2, by * 2 + 1), (bx * 2 + 1, by * 2 + 1)] {
                let [r, g, b] = at(img, x, y);
                let yv = 0.299 * r + 0.587 * g + 0.114 * b;
                cb += (b - yv) * 0.564 / 4.0;
                cr += (r - yv) * 0.713 / 4.0;
            }
            (cb, cr)
        };
        let one_way = |p: &[u8], q: &[u8], x: u32, y: u32| {
            let l = luma(p, x, y);
            let near_luma = (y - 1..=y + 1).any(|v| (x - 1..=x + 1).any(|u| (luma(q, u, v) - l).abs() <= 24.0));
            let (bx, by) = (x / 2, y / 2);
            let c = chroma(p, bx, by);
            let near_colour = (by - 1..=by + 1).any(|v| {
                (bx - 1..=bx + 1).any(|u| {
                    let d = chroma(q, u, v);
                    (d.0 - c.0).abs() <= 20.0 && (d.1 - c.1).abs() <= 20.0
                })
            });
            near_luma && near_colour
        };
        let (mut bad, mut n, mut worst) = (0, 0, vec![]);
        for y in (4..H - 4).step_by(3) {
            for x in (4..W - 4).step_by(3) {
                n += 1;
                if !(one_way(a, b, x, y) && one_way(b, a, x, y)) {
                    bad += 1;
                    if worst.len() < 8 {
                        let px = |img: &[u8]| at(img, x, y).map(|v| v as u8);
                        worst.push((x, y, px(a), px(b)));
                    }
                }
            }
        }
        (bad, n, worst)
    }

    /// Whether `c` is within 40 of `want` in every channel.
    pub fn near(c: [u8; 3], want: [u8; 3]) -> bool {
        c.iter().zip(want).all(|(a, b)| a.abs_diff(b) <= 40)
    }
}
