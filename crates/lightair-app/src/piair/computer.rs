//! PiAir's hands (piair/docs/design.md §6–7): every `computer.*` call passes Lightair's gates
//! (AI drives on, the human not touching anything, the app allowed, no password field, destructive
//! actions confirmed) before anything moves. Coordinates arrive in the pixels of the last
//! screenshot and are mapped back to display points here. Only macOS has the hands; elsewhere
//! every call is refused, and what only the hands use is unused.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
#[cfg(target_os = "macos")]
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

#[cfg(target_os = "macos")]
use base64::Engine as _;
use serde_json::Value;
#[cfg(target_os = "macos")]
use serde_json::json;

#[cfg(target_os = "macos")]
use super::input_mac as imp;
#[cfg(target_os = "macos")]
use imp::Button;

/// The human stays in charge this long after touching the mouse or keyboard.
pub const HUMAN_PAUSE: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub struct Refusal {
    pub code: &'static str,
    pub message: String,
}

fn refuse(code: &'static str, message: impl Into<String>) -> Refusal {
    Refusal { code, message: message.into() }
}

/// The last screenshot's geometry: display (index, origin and size in points) and the picture's
/// size, to map the model's pixels back to points.
#[derive(Clone, Copy, Debug)]
struct Shot {
    origin: (f32, f32),
    points: (f32, f32),
    pixels: (u32, u32),
}

pub struct Computer {
    /// AI-drives mode (off by default; the panel turns it on for this session).
    pub ai_drives: AtomicBool,
    shot: Mutex<Option<Shot>>,
    /// Bundle ids (macOS) or executable names (Windows) the AI may act in.
    pub allowed: Mutex<Vec<String>>,
    /// Actions in a row, for the rate limit.
    recent: Mutex<Vec<Instant>>,
}

/// Key combinations that quit, close or delete: the streamer confirms them.
const DESTRUCTIVE_KEYS: &[&str] = &[
    "cmd+q",
    "cmd+w",
    "cmd+shift+w",
    "cmd+delete",
    "cmd+backspace",
    "cmd+shift+delete",
    "cmd+shift+backspace",
    "cmd+alt+escape",
    "alt+f4",
    "ctrl+w",
    "shift+delete",
    "ctrl+shift+delete",
    "cmd+opt+escape",
];
/// Words on a button that mean "can't take it back".
const DESTRUCTIVE_WORDS: &[&str] = &[
    "delete",
    "remove",
    "erase",
    "discard",
    "quit",
    "send",
    "post",
    "publish",
    "buy",
    "pay",
    "purchase",
    "confirm",
    "empty trash",
    "format",
    "uninstall",
    "sign out",
    "log out",
    "end stream",
];

impl Default for Computer {
    fn default() -> Self {
        Computer {
            ai_drives: AtomicBool::new(false),
            shot: Mutex::new(None),
            allowed: Mutex::new(Vec::new()),
            recent: Mutex::new(Vec::new()),
        }
    }
}

fn num(p: &Value, k: &str) -> Option<f32> {
    p.get(k).and_then(Value::as_f64).map(|v| v as f32)
}

impl Computer {
    /// Answers one `computer.*` call. `ask` puts a yes/no question to the streamer (the panel)
    /// and waits for it.
    #[cfg(target_os = "macos")]
    pub fn call(&self, method: &str, p: &Value, ask: &dyn Fn(String) -> bool) -> Result<Value, Refusal> {
        self.call_mac(method, p, ask)
    }

    /// Answers one `computer.*` call: not available on this system yet.
    #[cfg(not(target_os = "macos"))]
    pub fn call(&self, method: &str, p: &Value, ask: &dyn Fn(String) -> bool) -> Result<Value, Refusal> {
        let _ = (method, p, ask);
        Err(refuse("unsupported", "Computer use isn't available on this system yet."))
    }

    #[cfg(target_os = "macos")]
    fn call_mac(&self, method: &str, p: &Value, ask: &dyn Fn(String) -> bool) -> Result<Value, Refusal> {
        match method {
            // a picture of the screen goes to a cloud model: only while the streamer lets the AI
            // drive, and only the allowed apps' windows are in it
            "computer.screenshot" => {
                if !self.ai_drives.load(Ordering::Relaxed) {
                    return Err(refuse(
                        "not_enabled",
                        "AI drives is off. The streamer turns it on in the PiAir panel.",
                    ));
                }
                return self.screenshot(p);
            }
            "computer.cursor" => {
                let (x, y) = self.to_pixels(imp::pointer());
                return Ok(json!({"x": x, "y": y}));
            }
            _ => {}
        }
        self.gate()?;
        let started = Instant::now();
        // stops between steps when the human touches anything, or AI drives goes off
        let stop =
            || !self.ai_drives.load(Ordering::Relaxed) || imp::since_human().is_some_and(|d| d < started.elapsed());
        let result = match method {
            "computer.move" => {
                let to = self.point(p)?;
                self.glide(to, None, num(p, "ms").map(|m| m as u32), &stop)?;
                Ok(json!({}))
            }
            "computer.click" => {
                let to = self.point(p)?;
                self.check_target(to, ask)?;
                let button = match p.get("button").and_then(Value::as_str) {
                    Some("right") => Button::Right,
                    Some("middle") => Button::Middle,
                    _ => Button::Left,
                };
                let count = p.get("count").and_then(Value::as_i64).unwrap_or(1).clamp(1, 3);
                let flags = p.get("modifiers").and_then(Value::as_array).map_or(0, |m| {
                    m.iter().filter_map(|v| v.as_str().and_then(imp::modifier_flag)).fold(0, |a, b| a | b)
                });
                self.glide(to, None, None, &stop)?;
                std::thread::sleep(Duration::from_millis(60));
                for c in 1..=count {
                    if stop() {
                        return Err(refuse("paused", "The streamer took over; stopped."));
                    }
                    imp::button(to, button, true, c, flags);
                    std::thread::sleep(Duration::from_millis(45));
                    imp::button(to, button, false, c, flags);
                    std::thread::sleep(Duration::from_millis(70));
                }
                Ok(json!({}))
            }
            "computer.drag" => {
                let from = self.point(p.get("from").unwrap_or(&Value::Null))?;
                let to = self.point(p.get("to").unwrap_or(&Value::Null))?;
                self.check_target(from, ask)?;
                self.glide(from, None, None, &stop)?;
                imp::button(from, Button::Left, true, 1, 0);
                std::thread::sleep(Duration::from_millis(80));
                let r = self.glide(to, Some(Button::Left), None, &stop);
                imp::button(imp::pointer(), Button::Left, false, 1, 0);
                r?;
                Ok(json!({}))
            }
            "computer.scroll" => {
                let at = self.point(p)?;
                self.check_target(at, &|_| true)?;
                self.glide(at, None, None, &stop)?;
                let (dx, dy) = (
                    p.get("dx").and_then(Value::as_i64).unwrap_or(0) as i32,
                    p.get("dy").and_then(Value::as_i64).unwrap_or(0) as i32,
                );
                // a line at a time, so it looks like a wheel
                let steps = dx.abs().max(dy.abs()).clamp(1, 40);
                for _ in 0..steps {
                    if stop() {
                        return Err(refuse("paused", "The streamer took over; stopped."));
                    }
                    imp::scroll(dx.signum() * (dx.abs() > 0) as i32, dy.signum() * (dy.abs() > 0) as i32);
                    std::thread::sleep(Duration::from_millis(16));
                }
                Ok(json!({}))
            }
            "computer.type" => {
                let text = p.get("text").and_then(Value::as_str).unwrap_or("");
                self.check_front()?;
                if imp::password_focused() {
                    return Err(refuse(
                        "password_field",
                        "A password field has the keyboard; Lightair never types into one.",
                    ));
                }
                imp::type_text(text, &stop);
                if stop() {
                    return Err(refuse("paused", "The streamer took over; stopped."));
                }
                Ok(json!({}))
            }
            "computer.key" => {
                let keys = p.get("keys").and_then(Value::as_str).unwrap_or("").to_lowercase().replace(' ', "");
                self.check_front()?;
                let parts: Vec<&str> = keys.split('+').filter(|s| !s.is_empty()).collect();
                let Some((last, mods)) = parts.split_last() else { return Err(refuse("invalid", "No keys given.")) };
                let flags = mods
                    .iter()
                    .map(|m| imp::modifier_flag(m).ok_or_else(|| refuse("invalid", format!("Unknown modifier {m}"))))
                    .collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .fold(0, |a, b| a | b);
                let code = imp::key_code(last).ok_or_else(|| refuse("invalid", format!("Unknown key {last}")))?;
                if flags & (imp::FLAG_COMMAND | imp::FLAG_CONTROL) == 0 && imp::password_focused() {
                    return Err(refuse(
                        "password_field",
                        "A password field has the keyboard; Lightair never types into one.",
                    ));
                }
                let normal = normalize_combo(&keys);
                if DESTRUCTIVE_KEYS.contains(&normal.as_str()) && !ask(format!("PiAir wants to press {keys}")) {
                    return Err(refuse("declined", "The streamer said no."));
                }
                imp::key(code, true, flags);
                std::thread::sleep(Duration::from_millis(40));
                imp::key(code, false, flags);
                Ok(json!({}))
            }
            "computer.zoom_to" => {
                let at = self.point(p)?;
                let hold = num(p, "hold_s").unwrap_or(4.0).clamp(0.5, 15.0);
                lightair_host::zoom_to::zoom_to(at, num(p, "scale"), Duration::from_secs_f32(hold));
                Ok(json!({}))
            }
            _ => Err(refuse("unknown", format!("No method {method}"))),
        };
        if result.is_err() && stop() && imp::since_human().is_some_and(|d| d < started.elapsed()) {
            return Err(refuse("paused", "The streamer took over; stopped."));
        }
        result
    }

    /// AI drives is on, the human isn't busy, not too fast.
    #[cfg(target_os = "macos")]
    fn gate(&self) -> Result<(), Refusal> {
        if !self.ai_drives.load(Ordering::Relaxed) {
            return Err(refuse("not_enabled", "AI drives is off. The streamer turns it on in the PiAir panel."));
        }
        if !imp::watch() {
            return Err(refuse(
                "not_enabled",
                "Lightair can't watch the mouse and keyboard (Input Monitoring), so it won't let the AI drive.",
            ));
        }
        if !imp::can_post(false) {
            return Err(refuse("not_enabled", "Lightair needs the Accessibility permission to move the pointer."));
        }
        if imp::since_human().is_some_and(|d| d < HUMAN_PAUSE) {
            return Err(refuse("paused", "The streamer is using the mouse or keyboard; wait a few seconds."));
        }
        let mut recent = self.recent.lock().unwrap_or_else(|e| e.into_inner());
        recent.retain(|t| t.elapsed() < Duration::from_secs(1));
        if recent.len() >= 15 {
            return Err(refuse("busy", "Too many actions at once; slow down."));
        }
        recent.push(Instant::now());
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn allowed(&self, bundle: &str) -> bool {
        self.allowed.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|a| a.eq_ignore_ascii_case(bundle))
    }

    /// The frontmost app is on the list (and isn't Lightair).
    #[cfg(target_os = "macos")]
    fn check_front(&self) -> Result<(), Refusal> {
        let Some((pid, bundle, name)) = imp::frontmost() else {
            return Err(refuse("not_allowed_app", "Can't tell which app is in front."));
        };
        if pid as u32 == std::process::id() {
            return Err(refuse("not_allowed_app", "The AI never acts in Lightair itself."));
        }
        if !self.allowed(&bundle) {
            return Err(refuse("not_allowed_app", format!("{name} isn't on PiAir's list of allowed apps.")));
        }
        Ok(())
    }

    /// The app under a point is allowed, and a destructive-looking button is confirmed.
    #[cfg(target_os = "macos")]
    fn check_target(&self, at: (f32, f32), ask: &dyn Fn(String) -> bool) -> Result<(), Refusal> {
        let (pid, bundle, words) = imp::element_at(at);
        if pid as u32 == std::process::id() {
            return Err(refuse("not_allowed_app", "The AI never acts in Lightair itself."));
        }
        match bundle {
            Some(b) if self.allowed(&b) => {}
            Some(b) => return Err(refuse("not_allowed_app", format!("{b} isn't on PiAir's list of allowed apps."))),
            // the desktop or something unnamed: only if the front app is allowed
            None => self.check_front()?,
        }
        let lower = words.to_lowercase();
        if let Some(w) = DESTRUCTIVE_WORDS.iter().find(|w| contains_word(&lower, w)) {
            let label = words.split_whitespace().take(8).collect::<Vec<_>>().join(" ");
            if !ask(format!("PiAir wants to click “{label}” ({w})")) {
                return Err(refuse("declined", "The streamer said no."));
            }
        }
        Ok(())
    }

    /// Moves along a natural path, stopping if the human takes over.
    #[cfg(target_os = "macos")]
    fn glide(
        &self,
        to: (f32, f32),
        held: Option<Button>,
        ms: Option<u32>,
        stop: &dyn Fn() -> bool,
    ) -> Result<(), Refusal> {
        let from = imp::pointer();
        let seed = (Instant::now().elapsed().as_nanos() as u64) ^ ((from.0 as u64) << 20) ^ (to.1 as u64);
        let (pts, step) = super::path::path(from, to, ms, seed | 0x9E37);
        for pt in pts {
            if stop() {
                return Err(refuse("paused", "The streamer took over; stopped."));
            }
            imp::move_to(pt, held);
            std::thread::sleep(step);
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn point(&self, p: &Value) -> Result<(f32, f32), Refusal> {
        let (Some(x), Some(y)) = (num(p, "x"), num(p, "y")) else {
            return Err(refuse("invalid", "x and y are needed."));
        };
        let shot = self
            .shot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ok_or_else(|| refuse("invalid", "Take a screenshot first."))?;
        let kx = shot.points.0 / shot.pixels.0.max(1) as f32;
        let ky = shot.points.1 / shot.pixels.1.max(1) as f32;
        let x = x.clamp(0.0, shot.pixels.0 as f32 - 1.0);
        let y = y.clamp(0.0, shot.pixels.1 as f32 - 1.0);
        Ok((shot.origin.0 + x * kx, shot.origin.1 + y * ky))
    }

    #[cfg(target_os = "macos")]
    fn to_pixels(&self, at: (f32, f32)) -> (f32, f32) {
        match *self.shot.lock().unwrap_or_else(|e| e.into_inner()) {
            Some(s) => (
                (at.0 - s.origin.0) * s.pixels.0 as f32 / s.points.0.max(1.0),
                (at.1 - s.origin.1) * s.pixels.1 as f32 / s.points.1.max(1.0),
            ),
            None => at,
        }
    }

    /// The display under the pointer (or `display`), at most `max_width` wide, as JPEG.
    #[cfg(target_os = "macos")]
    fn screenshot(&self, p: &Value) -> Result<Value, Refusal> {
        let (index, bounds) = match p.get("display").and_then(Value::as_u64) {
            Some(i) => {
                (i as u32, imp::display_by_index(i as u32).ok_or_else(|| refuse("invalid", "No such display."))?)
            }
            None => imp::display_at(imp::pointer()),
        };
        let jpeg = imp::capture(index).map_err(|e| refuse("failed", format!("Couldn't take a screenshot: {e}")))?;
        let img = image::load_from_memory(&jpeg).map_err(|e| refuse("failed", e.to_string()))?;
        let max_w = p.get("max_width").and_then(Value::as_u64).unwrap_or(1280).clamp(320, 1920) as u32;
        let img =
            if img.width() > max_w { img.resize(max_w, u32::MAX, image::imageops::FilterType::Triangle) } else { img };
        let mut rgb = img.to_rgb8();
        self.black_out(&mut rgb, (bounds.origin.x as f32, bounds.origin.y as f32), bounds.size.width as f32);
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 78)
            .encode_image(&rgb)
            .map_err(|e| refuse("failed", e.to_string()))?;
        let shot = Shot {
            origin: (bounds.origin.x as f32, bounds.origin.y as f32),
            points: (bounds.size.width as f32, bounds.size.height as f32),
            pixels: (img.width(), img.height()),
        };
        *self.shot.lock().unwrap_or_else(|e| e.into_inner()) = Some(shot);
        let (cx, cy) = self.to_pixels(imp::pointer());
        Ok(json!({
            "image": base64::engine::general_purpose::STANDARD.encode(&out),
            "width": img.width(),
            "height": img.height(),
            "display": index,
            "cursor": {"x": cx.round(), "y": cy.round()},
        }))
    }
}

#[cfg(target_os = "macos")]
impl Computer {
    /// Leaves only what the allowed apps' windows show where nothing else covers them; the rest
    /// (other apps, Lightair, the menu bar, the Dock, the desktop) is black. `origin` and `width`
    /// are the display's frame in points.
    fn black_out(&self, img: &mut image::RgbImage, origin: (f32, f32), width: f32) {
        let k = img.width() as f32 / width.max(1.0);
        let me = std::process::id() as i32;
        let mut allowed_pid: std::collections::HashMap<i32, bool> = std::collections::HashMap::new();
        let windows: Vec<WinInfo> = imp::windows_on_screen()
            .into_iter()
            .map(|win| {
                let allowed = win.pid != me
                    && *allowed_pid
                        .entry(win.pid)
                        .or_insert_with(|| imp::bundle_id(win.pid).is_some_and(|b| self.allowed(&b)));
                let f = win.frame;
                WinInfo {
                    frame: Rect {
                        x: f.origin.x as f32,
                        y: f.origin.y as f32,
                        w: f.size.width as f32,
                        h: f.size.height as f32,
                    },
                    allowed,
                    layer: win.layer,
                }
            })
            .collect();
        let display = Rect { x: origin.0, y: origin.1, w: width, h: img.height() as f32 / k };
        let cg = imp::CGRect {
            origin: imp::CGPoint { x: display.x as f64, y: display.y as f64 },
            size: imp::CGSize { width: display.w as f64, height: display.h as f64 },
        };
        let visible = imp::visible_frame(cg).map(|v| Rect {
            x: v.origin.x as f32,
            y: v.origin.y as f32,
            w: v.size.width as f32,
            h: v.size.height as f32,
        });
        let keep = mask(&windows, display, visible, (img.width(), img.height()));
        for (i, px) in img.pixels_mut().enumerate() {
            if !keep[i] {
                *px = image::Rgb([0, 0, 0]);
            }
        }
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
/// A rectangle in global display points (top-left origin).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
/// A window on screen as the screenshot mask sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WinInfo {
    pub frame: Rect,
    /// Its app is on the allowed list (and isn't Lightair).
    pub allowed: bool,
    /// 0 for a normal window; the menu bar, the Dock and overlays sit higher.
    pub layer: i32,
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
/// Which pixels of a `size` picture of `display` may go to the model, row by row: only the
/// visible parts of allowed windows (`windows` front to back), never their round corners, never
/// anything outside `visible` (the display without the menu bar and the Dock). No `visible`:
/// nothing shows. A full-display overlay above the windows (not allowed) covers nothing.
pub fn mask(windows: &[WinInfo], display: Rect, visible: Option<Rect>, size: (u32, u32)) -> Vec<bool> {
    let (w, h) = (size.0 as i64, size.1 as i64);
    let mut keep = vec![false; (w * h) as usize];
    let Some(v) = visible else { return keep };
    let k = size.0 as f32 / display.w.max(1.0);
    let px = |x: f32, lo: bool| -> i64 {
        (if lo { ((x - display.x) * k).floor() } else { ((x - display.x) * k).ceil() } as i64).clamp(0, w)
    };
    let py = |y: f32, lo: bool| -> i64 {
        (if lo { ((y - display.y) * k).floor() } else { ((y - display.y) * k).ceil() } as i64).clamp(0, h)
    };
    // back to front: an allowed window shows; anything above it covers it again
    for win in windows.iter().rev() {
        let f = win.frame;
        let whole = f.w * k >= w as f32 * 0.95 && f.h * k >= h as f32 * 0.95;
        if win.layer != 0 && whole && !win.allowed {
            continue;
        }
        let (x0, y0, x1, y1) = (px(f.x, true), py(f.y, true), px(f.x + f.w, false), py(f.y + f.h, false));
        for y in y0..y1 {
            let row = (y * w) as usize;
            keep[row + x0 as usize..row + x1 as usize].fill(win.allowed);
        }
        // windows have round corners: what's behind them there stays black too
        if win.allowed {
            let r = ((12.0 * k).ceil() as i64).min((x1 - x0) / 2).min((y1 - y0) / 2);
            for dy in 0..r {
                for dx in 0..r {
                    let (cx, cy) = (r - dx, r - dy);
                    if cx * cx + cy * cy <= r * r {
                        continue;
                    }
                    for (x, y) in
                        [(x0 + dx, y0 + dy), (x1 - 1 - dx, y0 + dy), (x0 + dx, y1 - 1 - dy), (x1 - 1 - dx, y1 - 1 - dy)]
                    {
                        keep[(y * w + x) as usize] = false;
                    }
                }
            }
        }
    }
    // the menu bar and the Dock never show, whatever is under them
    let (vx0, vy0) =
        ((((v.x - display.x) * k).ceil() as i64).clamp(0, w), (((v.y - display.y) * k).ceil() as i64).clamp(0, h));
    let (vx1, vy1) = (
        (((v.x + v.w - display.x) * k).floor() as i64).clamp(0, w),
        (((v.y + v.h - display.y) * k).floor() as i64).clamp(0, h),
    );
    for y in 0..h {
        for x in 0..w {
            if x < vx0 || x >= vx1 || y < vy0 || y >= vy1 {
                keep[(y * w + x) as usize] = false;
            }
        }
    }
    keep
}

fn contains_word(text: &str, word: &str) -> bool {
    text.match_indices(word).any(|(i, _)| {
        let before = text[..i].chars().next_back().is_none_or(|c| !c.is_alphanumeric());
        let after = text[i + word.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric());
        before && after
    })
}

/// Modifiers in a fixed order: "shift+cmd+Q" → "cmd+shift+q".
fn normalize_combo(keys: &str) -> String {
    let parts: Vec<String> = keys
        .to_lowercase()
        .split('+')
        .map(|s| match s.trim() {
            "command" | "meta" => "cmd".to_string(),
            "option" | "opt" => "alt".to_string(),
            "control" => "ctrl".to_string(),
            s => s.to_string(),
        })
        .collect();
    let order = ["cmd", "ctrl", "alt", "shift"];
    let mut mods: Vec<&String> = parts.iter().filter(|p| order.contains(&p.as_str())).collect();
    mods.sort_by_key(|m| order.iter().position(|o| o == m));
    let rest: Vec<&String> = parts.iter().filter(|p| !order.contains(&p.as_str())).collect();
    mods.into_iter().chain(rest).cloned().collect::<Vec<_>>().join("+")
}

#[cfg(test)]
mod tests {
    // (not `super::*`: gpui's own `test` macro would come with it)
    use super::{Computer, DESTRUCTIVE_KEYS, DESTRUCTIVE_WORDS, Rect, WinInfo, contains_word, mask, normalize_combo};

    fn destructive(keys: &str) -> bool {
        DESTRUCTIVE_KEYS.contains(&normalize_combo(keys).as_str())
    }

    /// Quitting, closing and deleting need the streamer's yes, however the combination is written.
    #[test]
    fn destructive_key_combinations_are_caught_however_theyre_written() {
        for keys in ["cmd+q", "cmd+shift+w", "Shift+Cmd+W", "command+delete", "opt+cmd+escape", "ctrl+shift+delete", "shift+ctrl+delete", "ALT+F4", "shift+delete"] {
            assert!(destructive(keys), "{keys}");
        }
        for keys in ["cmd+t", "cmd+c", "return", "shift+tab", "cmd+shift+t"] {
            assert!(!destructive(keys), "{keys}");
        }
    }

    /// A button whose words mean "can't take it back" needs a yes; a word inside another word
    /// doesn't count.
    #[test]
    fn destructive_button_words_are_whole_words() {
        let flags = |title: &str| DESTRUCTIVE_WORDS.iter().any(|w| contains_word(&title.to_lowercase(), w));
        for title in ["Delete", "Move to Trash and Delete", "Send message", "Empty Trash", "Buy now", "End stream"] {
            assert!(flags(title), "{title}");
        }
        for title in ["Deleted items", "Undelete", "Sender settings", "Postpone", "Paypal help"] {
            assert!(!flags(title), "{title}");
        }
    }

    /// The AI acts nowhere until the streamer allows an app; then in that app only (bundle ids
    /// compared without case).
    #[cfg(target_os = "macos")]
    #[test]
    fn only_the_apps_on_the_list_are_allowed() {
        let c = Computer::default();
        assert!(!c.allowed("com.apple.TextEdit"), "nothing by default");
        c.allowed.lock().unwrap().push("com.apple.TextEdit".into());
        assert!(c.allowed("com.apple.textedit"));
        assert!(!c.allowed("com.apple.Safari"));
    }

    // The screenshot mask: a 100×100 picture of a 100×100-point display whose menu bar is the top
    // 10 points and whose Dock is the bottom 10.
    const DISPLAY: Rect = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
    const VISIBLE: Rect = Rect { x: 0.0, y: 10.0, w: 100.0, h: 80.0 };

    fn win(x: f32, y: f32, w: f32, h: f32, allowed: bool) -> WinInfo {
        WinInfo { frame: Rect { x, y, w, h }, allowed, layer: 0 }
    }

    /// Whether the pixel at (x, y) goes to the model.
    fn shows(keep: &[bool], x: usize, y: usize) -> bool {
        keep[y * 100 + x]
    }

    /// Only an allowed window shows; the desktop and other apps' windows are black.
    #[test]
    fn an_allowed_window_shows_and_other_apps_are_black() {
        let windows = [win(60.0, 20.0, 30.0, 30.0, false), win(10.0, 20.0, 40.0, 60.0, true)];
        let keep = mask(&windows, DISPLAY, Some(VISIBLE), (100, 100));
        assert!(shows(&keep, 30, 50), "inside the allowed window");
        assert!(!shows(&keep, 75, 35), "another app's window");
        assert!(!shows(&keep, 5, 50), "the desktop");
    }

    /// A window in front of an allowed one covers it: that part is black.
    #[test]
    fn a_window_covering_an_allowed_one_is_black() {
        let windows = [win(30.0, 40.0, 20.0, 20.0, false), win(10.0, 20.0, 80.0, 60.0, true)];
        let keep = mask(&windows, DISPLAY, Some(VISIBLE), (100, 100));
        assert!(!shows(&keep, 40, 50), "covered");
        assert!(shows(&keep, 70, 50), "the uncovered part shows");
    }

    /// The menu bar and the Dock (outside the visible frame) never show, even over an allowed
    /// full-screen window; a full-screen overlay above the windows covers nothing.
    #[test]
    fn the_menu_bar_and_the_dock_are_black() {
        let overlay = WinInfo { frame: DISPLAY, allowed: false, layer: 25 };
        let windows = [overlay, win(0.0, 0.0, 100.0, 100.0, true)];
        let keep = mask(&windows, DISPLAY, Some(VISIBLE), (100, 100));
        assert!(!shows(&keep, 50, 5), "the menu bar");
        assert!(!shows(&keep, 50, 95), "the Dock");
        assert!(shows(&keep, 50, 50), "the window, under the overlay");
    }

    /// Where the menu bar and the Dock are can't be told: nothing shows at all.
    #[test]
    fn with_no_visible_frame_everything_is_black() {
        let keep = mask(&[win(0.0, 0.0, 100.0, 100.0, true)], DISPLAY, None, (100, 100));
        assert!(keep.iter().all(|k| !k));
    }
}
