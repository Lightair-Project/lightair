//! Enhanced Broadcasting at Go live (docs/plans/twitch-dual-format.md): a Twitch destination
//! with it on asks Twitch's configuration service for its plan, checks it, and starts as the
//! planned encodes in one connection: the landscape canvas, and with Dual Format the vertical
//! canvas too. Without a usable plan it goes live as one encode, landscape only, and says why.
//!
//! For development and checks, `LIGHTAIR_MULTITRACK_PLAN=<file.json>` reads the plan from a file
//! instead of asking Twitch (point the destination at Stream Check to look at the stream), and
//! `LIGHTAIR_ENHANCED_URL` asks another service.

use std::collections::HashMap;
use std::sync::Arc;

use lightair_engine::outputs::{Multitrack, MultitrackVideo, OutputKey, StreamConfig};
use lightair_engine::Engine;
use lightair_live::enhanced_broadcasting::{self as eb, Ask, CanvasSize, Plan, Verdict};
use lightair_model::{Destination, Id, Workspace};
use parking_lot::Mutex;

/// What Go live needs to ask for a destination's plan.
#[derive(Debug, Clone)]
pub struct Request {
    /// The destination's name, for the warning prompt.
    name: String,
    /// The canvases in track order: the destination's (landscape), then the vertical one.
    canvases: Vec<(Id, CanvasSize)>,
    max_bitrate_kbps: Option<u32>,
    max_video_tracks: Option<u32>,
    vod_track: bool,
    rtmps: bool,
    /// The encoders libobs has here.
    encoders: Vec<String>,
}

/// How the last Go live of a destination went with Enhanced Broadcasting, for its row and sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    /// The plan's video encodes, and their bitrate together.
    pub tracks: usize,
    pub kbps: u32,
    /// The vertical canvas went too.
    pub dual: bool,
    /// The platform's caution, or why it went landscape only.
    pub note: Option<String>,
}

static SUMMARIES: Mutex<Option<HashMap<Id, Summary>>> = Mutex::new(None);

/// A plan the platform cautioned about, waiting for the person: go live with it anyway, or not.
struct Held {
    id: Id,
    name: String,
    words: String,
    start: Box<dyn FnOnce() + Send>,
}

static HELD: Mutex<Option<Held>> = Mutex::new(None);

/// The destination waiting on the person because its platform cautioned about the plan: its id,
/// its name, and the platform's words.
pub fn held() -> Option<(Id, String, String)> {
    HELD.lock().as_ref().map(|h| (h.id, h.name.clone(), h.words.clone()))
}

/// The person's answer to the caution: go live with the plan anyway, or leave that destination off.
pub fn answer(go: bool) {
    let Some(h) = HELD.lock().take() else { return };
    if go {
        (h.start)();
    } else {
        lightair_engine::outputs::mark_idle(OutputKey::Stream(h.id));
    }
}

/// The last Go live's summary for destination `id`.
pub fn summary(id: Id) -> Option<Summary> {
    SUMMARIES.lock().as_ref().and_then(|m| m.get(&id).cloned())
}

fn remember(id: Id, s: Summary) {
    SUMMARIES.lock().get_or_insert_with(HashMap::new).insert(id, s);
}

/// Whether `d` is a Twitch destination that can use Enhanced Broadcasting (over RTMP or RTMPS).
pub fn offered(d: &Destination) -> bool {
    crate::platforms::takes_vod_track(d) && (d.server.starts_with("rtmp://") || d.server.starts_with("rtmps://") || crate::platforms::is_auto(&d.server))
}

/// What to ask for `d` at Go live, when it has Enhanced Broadcasting on and its canvas is
/// landscape; `None`: it goes live as usual.
pub fn request(d: &Destination, workspace: &Workspace, encoders: &[String]) -> Option<Request> {
    let enhanced = d.options.enhanced.as_ref().filter(|_| offered(d))?;
    let size = |c: &lightair_model::Canvas| CanvasSize { width: c.width, height: c.height, fps_num: c.fps_num, fps_den: c.fps_den.max(1) };
    let main = d.canvas.and_then(|id| workspace.canvas(id)).unwrap_or(workspace.primary_canvas());
    if main.height > main.width {
        return None;
    }
    let mut canvases = vec![(main.id, size(main))];
    if let Some(v) = enhanced.vertical_canvas.and_then(|id| workspace.canvas(id)).filter(|c| c.height > c.width) {
        canvases.push((v.id, size(v)));
    }
    Some(Request {
        name: d.name.clone(),
        canvases,
        max_bitrate_kbps: enhanced.max_bitrate_kbps,
        max_video_tracks: enhanced.max_video_tracks,
        vod_track: d.vod_mix.is_some(),
        rtmps: d.server.starts_with("rtmps://"),
        encoders: encoders.to_vec(),
    })
}

/// Asks for the plan (blocking: call it off the UI thread) and starts the destination with it;
/// without a usable plan, as one encode on `single_canvas` (its own canvas).
pub fn go(engine: Arc<Engine>, cfg: StreamConfig, req: Request, single_canvas: Option<Id>) {
    let id = cfg.id;
    let ask = Ask {
        key: cfg.key.clone(),
        client: "lightair".into(),
        client_version: env!("CARGO_PKG_VERSION").into(),
        codecs: vec!["h264".into()],
        machine: crate::machine::now(),
        canvases: req.canvases.iter().map(|(_, s)| *s).collect(),
        max_bitrate_kbps: req.max_bitrate_kbps,
        max_video_tracks: req.max_video_tracks,
        vod_track: req.vod_track,
        audio_rate: 48_000,
        audio_channels: 2,
    };
    let plan = match std::env::var("LIGHTAIR_MULTITRACK_PLAN").ok().filter(|p| !p.is_empty()) {
        Some(path) => std::fs::read_to_string(&path).map_err(|e| format!("the plan file couldn't be read ({e})")).and_then(|t| eb::read(&t)),
        None => {
            let url = std::env::var("LIGHTAIR_ENHANCED_URL").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| eb::TWITCH.into());
            eb::fetch(&url, &ask)
        }
    };
    match decide(plan, &req, &cfg.key) {
        Decision::Plan { plan, server, key } => {
            let (multitrack, canvases) = tracks(&plan, &req);
            remember(
                id,
                Summary {
                    tracks: plan.video.len(),
                    kbps: plan.video_kbps(),
                    dual: req.canvases.len() > 1,
                    note: match &plan.verdict {
                        Verdict::Caution(w) => Some(w.clone()),
                        _ => None,
                    },
                },
            );
            let cfg = StreamConfig { server, key, ..cfg };
            let start = move || {
                engine.post(move |s| {
                    let videos: Option<Vec<_>> = canvases.iter().map(|c| s.reconciler.canvas_video(*c)).collect();
                    let result = match videos {
                        Some(v) => s.outputs.start_multitrack_stream(&cfg, &multitrack, &v),
                        None => Err("A canvas in the plan isn't there any more.".into()),
                    };
                    if let Err(e) = result {
                        tracing::error!(%e, "multitrack stream failed to start");
                        lightair_engine::outputs::mark_failed(OutputKey::Stream(id), &e);
                    }
                });
            };
            // a caution: the person decides (the prompt over the window), in the platform's words
            match plan.verdict {
                Verdict::Caution(words) => *HELD.lock() = Some(Held { id, name: req.name.clone(), words, start: Box::new(start) }),
                _ => start(),
            }
        }
        Decision::LandscapeOnly { note } => {
            tracing::warn!(%note, "going live without Enhanced Broadcasting");
            remember(id, Summary { tracks: 1, kbps: cfg.encoders.video_bitrate_kbps, dual: false, note: Some(note) });
            engine.post(move |s| {
                let video = single_canvas.and_then(|f| s.reconciler.canvas_video(f));
                if let Err(e) = s.outputs.start_stream(&cfg, video) {
                    tracing::error!(%e, "stream failed to start");
                    lightair_engine::outputs::mark_failed(OutputKey::Stream(id), &e);
                }
            });
        }
    }
}

/// What Go live does with the platform's answer, or with the failure to get one.
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Go live with the plan, at its server and with its key (the plan's id added).
    Plan { plan: Plan, server: String, key: String },
    /// Go live as one encode, landscape only; `note` says why, as the card shows it ("Sent
    /// landscape only: <the platform's words>.").
    LandscapeOnly { note: String },
}

/// Decides between the plan and landscape only. A refusal's own words are the reason (what
/// follows from an empty plan isn't news); a plan that can't run here (a canvas not sent, an
/// encoder this computer doesn't have, nowhere to send it) or no plan at all goes landscape only
/// too, saying why. `key`: the destination's stream key.
pub fn decide(plan: Result<Plan, String>, req: &Request, key: &str) -> Decision {
    let plan = plan.and_then(|p| {
        if let Verdict::Refused(why) = &p.verdict {
            return Err(why.trim_end_matches('.').to_string());
        }
        let problems = p.problems(req.canvases.len(), &req.encoders);
        if problems.is_empty() { Ok(p) } else { Err(problems.join("; ")) }
    });
    let plan = plan.and_then(|p| match p.connect(req.rtmps, key) {
        Some(at) => Ok((p, at)),
        None => Err("the plan has nowhere to send the stream".into()),
    });
    match plan {
        Ok((plan, (server, key))) => Decision::Plan { plan, server, key },
        Err(why) => Decision::LandscapeOnly { note: format!("Sent landscape only: {why}.") },
    }
}

/// The engine's tracks for `plan`, and each video track's canvas.
fn tracks(plan: &Plan, req: &Request) -> (Multitrack, Vec<Id>) {
    let mut canvases = Vec::new();
    let video = plan
        .video
        .iter()
        .map(|t| {
            let (canvas, size) = req.canvases[t.canvas];
            canvases.push(canvas);
            // a lower rate than the canvas's: every Nth frame (never a higher one)
            let fps_divisor = t
                .fps
                .filter(|(n, _)| *n > 0)
                .map_or(1, |(n, d)| ((size.fps_num as u64 * d as u64) / (n as u64 * size.fps_den as u64)).max(1) as u32);
            MultitrackVideo {
                encoder: t.encoder.clone(),
                width: t.width,
                height: t.height,
                fps_divisor,
                settings: t.settings.clone(),
                scale: t.scale.clone(),
            }
        })
        .collect();
    let tables: Option<Vec<_>> = plan.video.iter().map(|t| t.bitrate_table.clone()).collect();
    let multitrack = Multitrack {
        video,
        audio: plan.audio.iter().map(|a| a.settings.clone()).collect(),
        vod_audio: plan.vod_audio.iter().map(|a| a.settings.clone()).collect(),
        bitrate_table: tables.map(|t| serde_json::Value::Array(t).to_string()),
    };
    (multitrack, canvases)
}

#[cfg(test)]
#[path = "enhanced_tests.rs"]
mod tests;
