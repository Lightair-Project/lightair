//! PiAir's hands (piair/docs/design.md §6–7): every `computer.*` call passes Lightair's gates
//! (AI drives on, the human not touching anything, the app allowed, no password field, destructive
//! actions confirmed) before anything moves. Coordinates arrive in the pixels of the last
//! screenshot and are mapped back to display points here.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use serde_json::{Value, json};

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
    pub fn call(&self, method: &str, p: &Value, ask: &dyn Fn(String) -> bool) -> Result<Value, Refusal> {
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (method, p, ask);
            return Err(refuse("unsupported", "Computer use isn't available on this system yet."));
        }
        #[cfg(target_os = "macos")]
        self.call_mac(method, p, ask)
    }

    #[cfg(target_os = "macos")]
    fn call_mac(&self, method: &str, p: &Value, ask: &dyn Fn(String) -> bool) -> Result<Value, Refusal> {
        match method {
            "computer.screenshot" => return self.screenshot(p),
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
                lightair_engine::zoom_to::zoom_to(at, num(p, "scale"), Duration::from_secs_f32(hold));
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
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 78)
            .encode_image(&img.to_rgb8())
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
