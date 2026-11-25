//! An avatar source apart from how it's drawn: its settings (as the document keeps them, the
//! keys the source reads), the model read from its file (a VRM, a Live2D model, an Inochi2D or
//! Umamo puppet, or a PNGtuber) and each frame's pose from the face, the body, an expression and
//! the mic. A renderer reads the model in the background with [`load`], poses it every frame with
//! a [`Poser`], then draws what the model holds.
//!
//! Settings: `file`, `device`, `mirror` (on by default), `framing` ("head", "bust", "half",
//! "full"), `zoom`, `width`, `height`, `calibration` (JSON), `expression`, `expression_weight`,
//! `body` ("off", "upper", "full"), `show_tracking`, `mic` (a mixer track's id),
//! `push_to_talk_keys` and `revision` (raised when the file changed in place).

use std::path::Path;
use std::sync::Arc;

use glam::Quat;
use serde_json::{Value, json};

use crate::body::{BodyFrame, BodyMode, BodyState};
use crate::pose::{Avatar, Controls, Framing};
use crate::rig::{Calibration, Face, Measures, Smoother};
use crate::vrm::Model;
use crate::{inochi2d, live2d, uma};

/// An avatar source's settings.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    /// A .vrm, a Live2D .model3.json, an Inochi2D .inp/.inx, an Umamo .uma or a PNGtuber .lairava.
    pub file: String,
    /// The camera's unique id (none: no tracking).
    pub device: String,
    pub mirror: bool,
    pub framing: Framing,
    pub zoom: f32,
    pub width: u32,
    pub height: u32,
    pub calibration: Calibration,
    /// An expression's name ("happy"), or none.
    pub expression: String,
    pub expression_weight: f32,
    /// How much of the body follows (a VRM's; flat avatars follow the face only).
    pub body: BodyMode,
    /// The camera's picture in a corner, with what the trackers see marked on it.
    pub show_tracking: bool,
    /// A PNGtuber's mic: a mixer track's id.
    pub mic: String,
    /// Raised when the file changed in place: read it again.
    pub revision: i64,
    /// A PNGtuber talks only while its push to talk is held (it has keys for it).
    pub push_to_talk: bool,
}

impl Default for Settings {
    /// The source's defaults.
    fn default() -> Settings {
        Settings {
            file: String::new(),
            device: String::new(),
            mirror: true,
            framing: Framing::Bust,
            zoom: 1.0,
            width: 1080,
            height: 1080,
            calibration: Calibration::default(),
            expression: String::new(),
            expression_weight: 1.0,
            body: BodyMode::Upper,
            show_tracking: false,
            mic: String::new(),
            revision: 0,
            push_to_talk: false,
        }
    }
}

impl Settings {
    /// Reads the source's settings (a JSON object), with the source's defaults for keys it
    /// doesn't have, and its limits.
    pub fn from_json(v: &Value) -> Settings {
        let d = Settings::default();
        let st = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let int =
            |k: &str, or: i64| v.get(k).and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64))).unwrap_or(or);
        let float = |k: &str, or: f32| v.get(k).and_then(Value::as_f64).map_or(or, |x| x as f32);
        let flag = |k: &str, or: bool| v.get(k).and_then(Value::as_bool).unwrap_or(or);
        let framing = v.get("framing").and_then(Value::as_str).map_or(d.framing, Framing::from_name);
        let body = v.get("body").and_then(Value::as_str).map_or(d.body, BodyMode::from_name);
        Settings {
            file: st("file"),
            device: st("device"),
            mirror: flag("mirror", d.mirror),
            framing,
            zoom: float("zoom", d.zoom).clamp(0.25, 4.0),
            width: int("width", d.width as i64).clamp(16, 4096) as u32,
            height: int("height", d.height as i64).clamp(16, 4096) as u32,
            calibration: calibration_from_json(&st("calibration")).unwrap_or_default(),
            expression: st("expression"),
            expression_weight: float("expression_weight", d.expression_weight),
            body,
            show_tracking: flag("show_tracking", false),
            mic: st("mic"),
            revision: int("revision", 0),
            push_to_talk: !st("push_to_talk_keys").is_empty(),
        }
    }

    /// Whether the file is a flat avatar's (posed by the face only).
    pub fn is_flat(&self) -> bool {
        is_flat(&self.file)
    }

    /// The body mode in force: off for a flat avatar.
    pub fn body_mode(&self) -> BodyMode {
        if self.is_flat() { BodyMode::Off } else { self.body }
    }

    /// The face a measured frame shows, as this avatar makes it: calibrated, mirrored if set.
    pub fn face(&self, m: &Measures) -> Face {
        let f = self.calibration.face(m);
        if self.mirror { f.mirrored() } else { f }
    }
}

/// A calibration from its JSON (the `calibration` setting).
pub fn calibration_from_json(text: &str) -> Option<Calibration> {
    let v: Value = serde_json::from_str(text).ok()?;
    let f = |v: &Value| v.as_f64().map(|x| x as f32);
    let h = v["head"].as_array()?;
    Some(Calibration {
        head: Quat::from_xyzw(f(h.first()?)?, f(h.get(1)?)?, f(h.get(2)?)?, f(h.get(3)?)?).normalize(),
        eye: [f(&v["eye"][0])?, f(&v["eye"][1])?],
        brow: [f(&v["brow"][0])?, f(&v["brow"][1])?],
        mouth_width: f(&v["mouth_width"])?,
    })
}

/// A calibration's JSON (for the `calibration` setting).
pub fn calibration_json(c: &Calibration) -> String {
    json!({
        "head": [c.head.x, c.head.y, c.head.z, c.head.w],
        "eye": c.eye,
        "brow": c.brow,
        "mouth_width": c.mouth_width,
    })
    .to_string()
}

/// Whether a file is a flat avatar's (`.model3.json`, `.inp`, `.inx`, `.uma`, `.lairava`).
pub fn is_flat(file: &str) -> bool {
    is_live2d(file) || is_inochi2d(file) || is_uma(file) || is_png(file)
}

/// A PNGtuber (`.lairava`).
pub fn is_png(file: &str) -> bool {
    file.to_ascii_lowercase().ends_with(".lairava")
}

pub fn is_uma(file: &str) -> bool {
    file.to_ascii_lowercase().ends_with(".uma")
}

pub fn is_live2d(file: &str) -> bool {
    file.to_ascii_lowercase().ends_with(".model3.json")
}

pub fn is_inochi2d(file: &str) -> bool {
    let f = file.to_ascii_lowercase();
    f.ends_with(".inp") || f.ends_with(".inx")
}

/// A model's name from its file ("mao_pro" for mao_pro.model3.json).
pub fn model_name(path: &str) -> String {
    let file = Path::new(path).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    let lower = file.to_ascii_lowercase();
    for ext in [".model3.json", ".vrm", ".inp", ".inx", ".uma", ".lairava"] {
        if lower.ends_with(ext) {
            return file[..file.len() - ext.len()].to_string();
        }
    }
    file
}

/// A VRM model's expressions to choose from: the moods first (happy, angry, sad, relaxed,
/// surprised), then the artist's own, without the ones the face drives (vowels, blinks, looks).
pub fn vrm_expressions(m: &Model) -> Vec<String> {
    const MOODS: [&str; 5] = ["happy", "angry", "sad", "relaxed", "surprised"];
    const DRIVEN: [&str; 12] = [
        "aa",
        "ih",
        "ou",
        "ee",
        "oh",
        "blink",
        "blinkLeft",
        "blinkRight",
        "lookUp",
        "lookDown",
        "lookLeft",
        "lookRight",
    ];
    let mut own: Vec<String> = m
        .expressions
        .keys()
        .filter(|k| !MOODS.contains(&k.as_str()) && !DRIVEN.contains(&k.as_str()) && k.as_str() != "neutral")
        .cloned()
        .collect();
    own.sort();
    MOODS.iter().filter(|e| m.expressions.contains_key(**e)).map(|e| e.to_string()).chain(own).collect()
}

/// A PNGtuber: its runtime, its pictures (straight alpha; a renderer may take them once it has
/// made its textures) and the mic for the next frame.
pub struct Pngtuber {
    pub puppet: lightair_pngtuber::Puppet,
    pub pictures: Vec<lightair_pngtuber::Picture>,
    level_db: f32,
    push_to_talk: Option<bool>,
}

impl Pngtuber {
    pub fn load(path: &Path) -> Result<Pngtuber, String> {
        let loaded = lightair_pngtuber::file::read_path(path).map_err(|e| e.to_string())?;
        Ok(Pngtuber {
            puppet: lightair_pngtuber::Puppet::new(loaded.avatar),
            pictures: loaded.pictures,
            level_db: f32::NEG_INFINITY,
            push_to_talk: None,
        })
    }

    /// The mic's level (dBFS) and push to talk (held or not; `None` without keys), for the next
    /// frame.
    pub fn set_mic(&mut self, level_db: f32, push_to_talk: Option<bool>) {
        self.level_db = level_db;
        self.push_to_talk = push_to_talk;
    }

    pub fn update(&mut self, expression: Option<&str>, dt: f32) {
        self.puppet.step(&lightair_pngtuber::Inputs {
            level_db: self.level_db,
            push_to_talk: self.push_to_talk,
            expression,
            dt,
        });
    }

    /// The part of the canvas a framing shows, at an aspect (width / height): left, right, top,
    /// bottom (canvas pixels, y up, as the other flat avatars' frames).
    pub fn frame(&self, framing: Framing, aspect: f32, zoom: f32) -> (f32, f32, f32, f32) {
        let a = self.puppet.avatar();
        let (w, h) = (a.width as f32, a.height as f32);
        let share = match framing {
            Framing::Head => 0.5,
            Framing::Bust => 0.7,
            Framing::Half => 0.85,
            Framing::Full => 1.0,
        };
        let fh = h * share * 1.02 / zoom.clamp(0.25, 4.0);
        let top = h + h * 0.01;
        let fw = fh * aspect.max(0.05);
        (w * 0.5 - fw * 0.5, w * 0.5 + fw * 0.5, top, top - fh)
    }
}

/// A flat (2D) avatar: a Live2D model, an Inochi2D or Umamo puppet or a PNGtuber, posed by the
/// face only (a PNGtuber by the mic).
pub enum Flat {
    Live2d(Box<live2d::Puppet>),
    Inochi2d(Box<inochi2d::Puppet>),
    Uma(Box<uma::Puppet>),
    Png(Box<Pngtuber>),
}

impl Flat {
    pub fn load(path: &Path) -> Result<Flat, String> {
        let file = path.to_string_lossy();
        if is_png(&file) {
            Pngtuber::load(path).map(|p| Flat::Png(Box::new(p)))
        } else if is_inochi2d(&file) {
            inochi2d::Puppet::load(path).map(|p| Flat::Inochi2d(Box::new(p))).map_err(|e| e.to_string())
        } else if is_uma(&file) {
            uma::Puppet::load(path).map(|p| Flat::Uma(Box::new(p))).map_err(|e| e.to_string())
        } else {
            live2d::Puppet::load(path).map(|p| Flat::Live2d(Box::new(p))).map_err(|e| e.to_string())
        }
    }

    /// The model's expressions: a Live2D model's own; for an Inochi2D or Umamo puppet (whose files
    /// carry none), the parameters the face doesn't drive, each held at its maximum when chosen; a
    /// PNGtuber's.
    pub fn expressions(&self) -> Vec<String> {
        match self {
            Flat::Live2d(p) => p.expression_names(),
            Flat::Inochi2d(p) => p.toggles(),
            Flat::Uma(p) => p.toggles(),
            Flat::Png(p) => p.puppet.expressions(),
        }
    }

    /// Which of its expressions are costumes (a PNGtuber's; the others have none).
    pub fn costumes(&self) -> Vec<String> {
        match self {
            Flat::Png(p) => {
                p.puppet.avatar().expressions.iter().filter(|e| e.costume).map(|e| e.name.clone()).collect()
            }
            _ => Vec::new(),
        }
    }

    /// A PNGtuber's mic: its level (dBFS) and push to talk, for the next frame.
    pub fn set_mic(&mut self, level_db: f32, push_to_talk: Option<bool>) {
        if let Flat::Png(p) = self {
            p.set_mic(level_db, push_to_talk);
        }
    }

    /// How strongly a Live2D model's expression shows (`0..=1`); the others hold theirs at full.
    pub fn set_expression_weight(&mut self, weight: f32) {
        if let Flat::Live2d(p) = self {
            p.set_expression_weight(weight);
        }
    }

    /// Poses it for this frame, with an expression (its name) or none.
    pub fn update(&mut self, face: &Face, expression: Option<&str>, dt: f32) {
        match self {
            Flat::Live2d(p) => p.update(face, expression, dt),
            Flat::Inochi2d(p) => {
                p.hold(expression);
                p.update(face, dt)
            }
            Flat::Uma(p) => {
                p.update(face, dt);
                let max = expression.and_then(|e| p.parameters().iter().find(|x| x.id == e).map(|x| (e, x.max)));
                if let Some((e, max)) = max {
                    p.set_parameter(e, max);
                    p.pose();
                }
            }
            Flat::Png(p) => p.update(expression, dt),
        }
    }

    /// The part of the model a framing shows, at an aspect (width / height): left, right, top,
    /// bottom (the model's units, y up).
    pub fn frame(&self, framing: Framing, aspect: f32, zoom: f32) -> (f32, f32, f32, f32) {
        match self {
            Flat::Live2d(p) => p.frame(framing, aspect, zoom),
            Flat::Inochi2d(p) => p.frame(framing, aspect, zoom),
            Flat::Uma(p) => p.frame(framing, aspect, zoom),
            Flat::Png(p) => p.frame(framing, aspect, zoom),
        }
    }
}

/// A model ready to pose.
pub enum Posable {
    Vrm(Box<Avatar>),
    Flat(Box<Flat>),
}

/// A model read from its file, with what the Inspector shows of it.
pub struct Loaded {
    pub model: Posable,
    /// The model's own name, or its file's.
    pub name: String,
    pub expressions: Vec<String>,
    /// Which of its expressions are costumes (a PNGtuber's).
    pub costumes: Vec<String>,
}

/// Reads an avatar's file (slow: call it off the drawing thread).
pub fn load(file: &str) -> Result<Loaded, String> {
    if is_flat(file) {
        let flat = Flat::load(Path::new(file))?;
        Ok(Loaded {
            name: model_name(file),
            expressions: flat.expressions(),
            costumes: flat.costumes(),
            model: Posable::Flat(Box::new(flat)),
        })
    } else {
        let bytes = std::fs::read(file).map_err(|e| e.to_string())?;
        let m = Model::load(&bytes).map_err(|e| e.to_string())?;
        let name = if m.meta.name.is_empty() { model_name(file) } else { m.meta.name.clone() };
        let expressions = vrm_expressions(&m);
        Ok(Loaded { name, expressions, costumes: Vec::new(), model: Posable::Vrm(Box::new(Avatar::new(Arc::new(m)))) })
    }
}

/// What moves the avatar this frame.
#[derive(Clone, Debug)]
pub struct Input<'a> {
    /// The face seen this frame, as the avatar makes it (see [`Settings::face`]); `None` when
    /// none is seen (the avatar settles back to neutral).
    pub face: Option<Face>,
    /// The body seen this frame (a VRM's body tracking).
    pub body: Option<&'a BodyFrame>,
    /// An expression and its weight over the settings' own (a co-host's mood).
    pub expression: Option<(&'a str, f32)>,
    /// A PNGtuber's mic level now (dBFS; `-inf` for silence).
    pub mic_db: f32,
    /// A PNGtuber's push to talk is held.
    pub push_to_talk_held: bool,
}

impl Default for Input<'_> {
    /// Nothing seen, no expression over the settings', silence.
    fn default() -> Self {
        Input { face: None, body: None, expression: None, mic_db: f32::NEG_INFINITY, push_to_talk_held: false }
    }
}

/// Poses a model frame by frame: the face smoothed (and let settle when it's lost), the body
/// followed, the expression and mic applied.
#[derive(Default)]
pub struct Poser {
    smoother: Smoother,
    body: BodyState,
}

impl Poser {
    /// The body as it's followed now.
    pub fn body(&self) -> &BodyState {
        &self.body
    }

    /// Poses `model` for this frame, `dt` seconds after the last.
    pub fn step(&mut self, model: &mut Posable, settings: &Settings, input: &Input, dt: f32) {
        let face = self.smoother.step(input.face, dt);
        let expression: Option<(&str, f32)> = input.expression.or_else(|| {
            (!settings.expression.is_empty()).then_some((settings.expression.as_str(), settings.expression_weight))
        });
        self.body.step(input.body, settings.mirror, dt);
        match model {
            Posable::Vrm(a) => {
                let (bones, torso) = a.body_bones(&self.body, settings.body);
                a.update(&Controls { face, expression: expression.map(|(e, w)| (e.to_owned(), w)), bones, torso }, dt);
            }
            Posable::Flat(f) => {
                f.set_expression_weight(expression.map_or(1.0, |e| e.1));
                f.set_mic(input.mic_db, settings.push_to_talk.then_some(input.push_to_talk_held));
                f.update(&face, expression.map(|e| e.0), dt);
            }
        }
    }
}
