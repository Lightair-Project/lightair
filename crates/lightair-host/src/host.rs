//! The document host: the Lightair document as it runs without libobs. It takes the same
//! patches the libobs reconciler takes (`lightair_model::diff`), on the engine thread, and keeps
//! what the document needs running besides Lightair's own compositor:
//!
//! - **The show:** the workspace as last applied, published for the compositors
//!   (`crate::show::publish`) after every change, and the scene switch each program change
//!   starts (`crate::show::set_switch`).
//! - **The mixer:** the audio engine (`crate::audio_host::AudioHost`), started with the first
//!   track: audio interfaces and microphones opened by the engine itself, every other track's
//!   source taken from the runtime that feeds it by id, monitoring, mixes on devices of their
//!   own, meters, plug-ins and multitrack files.
//! - **A runtime per source that needs one**, made from the workspace, keyed by the source's id,
//!   updated when its settings change and dropped when it goes: media files, web pages, NDI
//!   receivers, desktop and app sound, the co-host's voice, timers, the soundboard, links from
//!   another computer. Whether each is on air follows the program, the overlays, a switch under
//!   way and the scenes drawn for outputs alone.
//!
//! What has no runtime without libobs yet (avatars' posing, phone cameras' decoding, playlists,
//! media files AVFoundation can't play) is logged once per kind; the methods that would reach it
//! return nothing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use lightair_model::{Id, Patch, Source, TransitionKind, TransitionSettings, Workspace, kinds};
use parking_lot::Mutex;

use crate::audio_host::{AudioHost, MultitrackReport};
use crate::media::{MediaAction, MediaStatus};
use crate::meters::meters;
use crate::timers::{TIMER_KIND, TimerAction, TimerOut, TimerStatus, Timers};

// ---------------------------------------------------------------- avatars (not here yet)

pub use crate::avatar::{Status as AvatarStatus, status_of as avatar_status_of};
pub use crate::avatar_pins::Anchor;
pub use crate::avatar_pins::Spot;

// ---------------------------------------------------------------- what a source needs

/// The runtime a source needs, from its kind and settings. Two sources needing the same thing
/// keep their runtime when settings change; anything else makes a new one.
#[derive(Debug, Clone, PartialEq)]
enum Need {
    /// Nothing here: the compositor draws it alone (images, text, colours, screens, cameras), or
    /// it's nothing that runs.
    Nothing,
    Timer,
    /// A media file AVFoundation plays (macOS).
    Media,
    /// The soundboard: a media player whose file the pads set (macOS).
    Soundboard,
    Web,
    /// An NDI source, by its name.
    Ndi(String),
    /// Desktop sound (`None`) or one app's (macOS).
    SystemSound(Option<String>),
    Voice,
    /// A link from another computer the compositor receives: its sound comes through a slot.
    Link,
    /// A phone camera: its streams decoded (macOS).
    Phone,
    /// An avatar: its camera tracked and its posing published (macOS).
    Avatar,
    /// A camera over USB (libgphoto2's live view, macOS).
    Dslr,
    /// A media file or stream AVFoundation doesn't play: FFmpeg decodes it (macOS).
    Ffmpeg,
    /// An audio input the mixer opens itself: its device setting ("" or "default": the system's).
    Device(String),
    /// Something that has no runtime without libobs yet (what it is, for the log).
    Missing(&'static str),
}

fn text(settings: &serde_json::Value, key: &str) -> String {
    settings[key].as_str().unwrap_or_default().to_string()
}

/// Whether a media source's file plays through AVFoundation: a local MP4 or QuickTime file, or a
/// sound file AVFoundation reads (MP3, AAC, WAV, AIFF, CAF, FLAC), at normal speed.
fn native_media(settings: &serde_json::Value) -> bool {
    let local = settings.get("is_local_file").and_then(serde_json::Value::as_bool) != Some(false);
    let speed = settings.get("speed_percent").and_then(serde_json::Value::as_i64).unwrap_or(100);
    let file = settings.get("local_file").and_then(serde_json::Value::as_str).unwrap_or("");
    let ext = std::path::Path::new(file).extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    local
        && speed == 100
        && matches!(
            ext.as_deref(),
            Some("mp4" | "m4v" | "mov" | "mp3" | "m4a" | "aac" | "wav" | "aif" | "aiff" | "caf" | "flac")
        )
}

/// Whether a media source plays a playlist (several files one after another).
fn playlist(settings: &serde_json::Value) -> bool {
    settings["playlist"].as_array().is_some_and(|a| a.iter().any(|v| v.as_str().is_some_and(|s| !s.is_empty())))
}

fn need(source: &Source) -> Need {
    let s = &source.settings;
    let mac = cfg!(target_os = "macos");
    match source.kind.as_str() {
        TIMER_KIND => Need::Timer,
        crate::soundboard::KIND if mac => Need::Soundboard,
        crate::soundboard::KIND => Need::Missing("soundboard"),
        "ffmpeg_source" if crate::show::link_received_by_compositor(source) => Need::Link,
        "ffmpeg_source" if mac && playlist(s) => Need::Media,
        "ffmpeg_source" if playlist(s) => Need::Missing("media playlist"),
        "ffmpeg_source" if mac && native_media(s) => Need::Media,
        "ffmpeg_source" if mac => Need::Ffmpeg,
        "ffmpeg_source" => Need::Missing("media file"),
        "lightair_web" if cfg!(any(target_os = "macos", windows)) => Need::Web,
        crate::ndi::KIND => Need::Ndi(text(s, "ndi_name")),
        crate::piair_voice::KIND => Need::Voice,
        crate::devices::DEVICE_KIND | kinds::MIC => Need::Device(text(s, "device")),
        // a loopback device (BlackHole and the like) is an input like any other
        kinds::DESKTOP_AUDIO if !crate::devices::is_system_default(&text(s, "device")) => {
            Need::Device(text(s, "device"))
        }
        kinds::DESKTOP_AUDIO if mac => Need::SystemSound(None),
        kinds::APP_AUDIO if mac => Need::SystemSound(Some(text(s, "app"))),
        kinds::DESKTOP_AUDIO | kinds::APP_AUDIO => Need::Missing("desktop or app sound"),
        crate::phone_camera::KIND if mac => Need::Phone,
        crate::phone_camera::KIND => Need::Missing("phone camera"),
        crate::avatar::KIND if cfg!(any(target_os = "macos", windows)) => Need::Avatar,
        crate::avatar::KIND => Need::Missing("avatar"),
        crate::dslr::KIND if mac => Need::Dslr,
        crate::dslr::KIND => Need::Missing("camera by USB"),
        _ => Need::Nothing,
    }
}

/// A web page source's page, from its settings (the libobs source's defaults where unset).
#[cfg(any(target_os = "macos", windows))]
fn page_options(s: &serde_json::Value) -> lightair_web::PageOptions {
    let int = |k: &str, default: i64| s[k].as_i64().unwrap_or(default);
    lightair_web::PageOptions {
        url: text(s, "url"),
        width: int("width", 1920).clamp(16, 7680) as u32,
        height: int("height", 1080).clamp(16, 7680) as u32,
        fps: int("fps", 30).clamp(1, 60) as u32,
        zoom: int("zoom", 100).clamp(25, 500) as f32 / 100.0,
        css: text(s, "css"),
    }
}

/// A timer's link to the host: whether its object is on air (an autostart timer starts then).
/// Its text goes to the compositor through `crate::show` only.
struct TimerOnAir(Arc<AtomicBool>);

impl TimerOut for TimerOnAir {
    fn text(&self, _: &str) {}

    fn on_air(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

/// One source's runtime.
enum Runtime {
    /// The timer lives in [`Host::timers`]; this says whether it's on air.
    Timer(Arc<AtomicBool>),
    #[cfg(target_os = "macos")]
    Media {
        player: crate::media_player::MediaPlayer,
        active: bool,
        /// Its files played in turn, when it has a playlist.
        playlist: Option<Playlist>,
    },
    #[cfg(target_os = "macos")]
    Soundboard {
        player: crate::media_player::MediaPlayer,
        playing: Mutex<(String, bool)>,
    },
    #[cfg(any(target_os = "macos", windows))]
    Web {
        page: crate::web::WebPage,
        shown: bool,
    },
    /// Held for its receiving thread (it stops as it drops); likewise the next two.
    Ndi {
        _receiver: crate::ndi::Receiver,
    },
    #[cfg(target_os = "macos")]
    SystemSound {
        _sound: crate::system_audio::SystemSound,
    },
    Voice {
        _voice: crate::piair_voice::Voice,
    },
    /// The sound slot is registered with the mixer by id (`crate::audio_host::feed_slot`).
    Link,
    #[cfg(target_os = "macos")]
    Phone(crate::phone_run::PhoneCamera),
    #[cfg(any(target_os = "macos", windows))]
    Avatar(crate::avatar_run::AvatarRuntime),
    #[cfg(target_os = "macos")]
    Ffmpeg {
        media: crate::ffmpeg_media::FfmpegMedia,
        active: bool,
    },
    /// Held for its thread (it stops as it drops).
    #[cfg(target_os = "macos")]
    Dslr {
        _live: crate::dslr::LiveView,
    },
}

/// A media source's playlist: its order and place, and when its file last started (a new file
/// reads as ended for a moment).
#[cfg(target_os = "macos")]
struct Playlist {
    order: crate::playlists::Order,
    started: Instant,
}

/// The player's settings for a playlist's file (never looping one file).
#[cfg(target_os = "macos")]
fn playlist_file(file: &str) -> crate::media_player::Settings {
    crate::media_player::Settings {
        path: file.to_string(),
        looping: false,
        restart_on_activate: false,
        clear_on_end: false,
    }
}

/// A scene switch under way: the scenes it draws besides the program until it ends.
struct Under {
    from: Id,
    /// The scene a Scene transition plays over it.
    over: Option<Id>,
    ends: Instant,
}

/// Logs once that `what` isn't available without libobs yet.
fn not_yet(what: &'static str) {
    static SEEN: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock();
    if !seen.contains(&what) {
        seen.push(what);
        tracing::debug!(what, "not available without libobs yet");
    }
}

// ---------------------------------------------------------------- the host

/// The document host, owned by the engine thread (the libobs engine's `Reconciler`).
pub struct Host {
    /// The transition the next program change uses instead of its scene's or the workspace's
    /// (a phone's `staging.take` with a transition), used once.
    pub next_transition: Option<TransitionSettings>,
    /// The workspace as last applied.
    workspace: Option<Arc<Workspace>>,
    program: Option<Id>,
    switch: Option<Under>,
    /// The scenes drawn for outputs alone, as last seen (scene, canvas).
    targets: Vec<(Id, Id)>,
    /// The primary canvas as last applied (kept across workspaces: outputs follow it).
    primary: Option<Id>,
    /// Each canvas's output shape, as last applied (kept while another workspace opens).
    shapes: HashMap<Id, (u32, u32, u32, u32)>,
    sample_rate: u32,
    /// The audio engine playing the mixer, started with the first track.
    audio: Option<AudioHost>,
    /// The direct monitor wanted: output device ("" default) and whether it plays the program
    /// (else the cue mix).
    monitor: Option<(String, bool)>,
    /// The monitor follows the monitoring device chosen (`crate::monitoring`): no device was
    /// given for direct monitoring, and the mixer is the only way to the headphones.
    monitor_follows: bool,
    /// What the compositors showed when the runtimes last looked (`crate::show::shown_changes`).
    shown_changes: u64,
    /// Frames per audio block (`None`: the default).
    audio_block: Option<usize>,
    /// What each source needs, and the runtimes made for it.
    needs: HashMap<Id, Need>,
    runtimes: HashMap<Id, Runtime>,
    timers: Timers,
    /// Sources on air now (the program, overlays, a switch under way, scenes drawn for outputs).
    active: HashSet<Id>,
    /// Sources held showing off air (to learn their size), until `release_sources`.
    held: HashSet<Id>,
    /// Kinds already logged as having no runtime yet.
    missing_logged: HashSet<&'static str>,
    /// Track sources already logged as not feeding the mixer.
    silent_logged: HashSet<Id>,
    /// The object being cropped (scene, canvas, object), kept for when the compositor shows it.
    crop_preview: Option<(Id, Id, Id)>,
}

impl Default for Host {
    fn default() -> Self {
        Self::new(48_000)
    }
}

impl Host {
    /// A host whose mixer runs at `sample_rate`.
    pub fn new(sample_rate: u32) -> Host {
        Host {
            next_transition: None,
            workspace: None,
            program: None,
            switch: None,
            targets: Vec::new(),
            primary: None,
            shapes: HashMap::new(),
            sample_rate,
            audio: None,
            monitor: None,
            monitor_follows: false,
            shown_changes: 0,
            audio_block: None,
            needs: HashMap::new(),
            runtimes: HashMap::new(),
            timers: Timers::default(),
            active: HashSet::new(),
            held: HashSet::new(),
            missing_logged: HashSet::new(),
            silent_logged: HashSet::new(),
            crop_preview: None,
        }
    }

    // ------------------------------------------------------------ the document

    /// Applies patches computed from two snapshots; `workspace` is the newer one.
    pub fn apply(&mut self, workspace: &Workspace, patches: &[Patch]) {
        self.follow_canvases(workspace);
        let mut audio_changed = false;
        for patch in patches {
            match patch {
                Patch::SourceAdded(source) => {
                    self.add_source(source);
                    audio_changed = true;
                }
                Patch::SourceChanged { after, .. } => audio_changed |= self.change_source(after),
                Patch::SourceRemoved(id) => {
                    self.remove_source(*id);
                    audio_changed = true;
                }
                Patch::SceneRemoved(id) if self.program == Some(*id) => self.set_program(None, workspace),
                Patch::ProgramChanged(program) => self.set_program(*program, workspace),
                Patch::TrackAdded(track) => {
                    if meters().get(track.id).is_none() {
                        meters().insert(track.id);
                    }
                    audio_changed = true;
                }
                Patch::TrackRemoved(id) => {
                    meters().remove(*id);
                    audio_changed = true;
                }
                Patch::TrackChanged { .. } | Patch::AudioSetup => audio_changed = true,
                // the compositor draws canvases and scenes from the published workspace
                _ => {}
            }
        }
        self.workspace = Some(Arc::new(workspace.clone()));
        if audio_changed {
            self.sync_audio(workspace);
        }
        self.sync_on_air();
        crate::show::publish(workspace);
    }

    /// Everything from `workspace`, from scratch (another workspace opened).
    pub fn rebuild(&mut self, workspace: &Workspace) {
        self.clear();
        let patches = lightair_model::diff(&workspace.empty_like(), workspace);
        self.apply(workspace, &patches);
    }

    /// Lets everything go: every runtime, the mixer, the show.
    pub fn clear(&mut self) {
        self.held.clear();
        let ids: Vec<Id> = self.runtimes.keys().copied().collect();
        for id in ids {
            self.drop_runtime(id);
        }
        self.timers.clear();
        if let Some(ws) = &self.workspace {
            for t in &ws.tracks {
                meters().remove(t.id);
            }
        }
        // stops the audio thread (and lets every captured source go)
        self.audio = None;
        self.needs.clear();
        self.active.clear();
        self.silent_logged.clear();
        self.program = None;
        self.switch = None;
        self.crop_preview = None;
        self.workspace = None;
        crate::show::clear();
    }

    /// Looks after what runs over time: timers' text, web pages' painting and closing, a switch
    /// ending, scenes drawn for outputs coming and going. The engine thread calls it about ten
    /// times a second; the app's `cleanup` too.
    pub(crate) fn tick(&mut self) {
        self.timers.tick();
        // a preview shows something else: what runs while shown follows
        let changes = crate::show::shown_changes();
        if changes != self.shown_changes {
            self.shown_changes = changes;
            self.sync_on_air();
        }
        // the monitoring device chosen changed: the monitor follows it
        if self.monitor_follows
            && let Some((device, program)) = self.monitor.clone()
        {
            let chosen = crate::monitoring::current().1;
            let chosen = if chosen == "default" { String::new() } else { chosen };
            if chosen != device {
                self.set_headphones(None, program);
            }
        }
        for rt in self.runtimes.values_mut() {
            match rt {
                #[cfg(any(target_os = "macos", windows))]
                Runtime::Web { page, .. } => page.tick(),
                #[cfg(target_os = "macos")]
                Runtime::Phone(phone) => phone.tick(),
                // a playlist's file ended: the next one
                #[cfg(target_os = "macos")]
                Runtime::Media { player, active, playlist: Some(p) } => {
                    if p.started.elapsed() >= Duration::from_millis(800)
                        && player.state() == crate::media_player::state::ENDED
                        && p.order.ended()
                        && let Some(file) = p.order.file()
                    {
                        player.update(playlist_file(file), *active);
                        player.restart();
                        p.started = Instant::now();
                    }
                }
                _ => {}
            }
        }
        let mut changed = false;
        if self.switch.as_ref().is_some_and(|s| Instant::now() >= s.ends) {
            self.switch = None;
            changed = true;
        }
        let targets = self.scene_targets();
        if targets != self.targets {
            self.targets = targets;
            changed = true;
        }
        if changed {
            self.sync_on_air();
        }
    }

    /// The app's periodic call (with the stats): looks after what runs over time ([`Self::tick`]).
    pub fn cleanup(&mut self) {
        self.tick();
    }

    /// Keeps the outputs on the primary canvas (another workspace opened: what took the old
    /// primary canvas's frames takes the new one's) and tells them each canvas's shape.
    fn follow_canvases(&mut self, workspace: &Workspace) {
        let Some(primary) = workspace.canvases.first().map(|c| c.id) else { return };
        if let Some(old) = self.primary.filter(|o| *o != primary && workspace.canvas(*o).is_none()) {
            #[cfg(target_os = "macos")]
            crate::own_outputs::canvas_moved(old, primary);
            #[cfg(not(target_os = "macos"))]
            let _ = old;
        }
        self.primary = Some(primary);
        let mut changed = false;
        for c in &workspace.canvases {
            let shape = crate::program::shape_of(c);
            changed |= self.shapes.insert(c.id, shape) != Some(shape);
        }
        if changed {
            crate::program::set_output_shapes(self.shapes.iter().map(|(id, s)| (*id, *s)).collect());
        }
    }

    /// Puts `program` on air: the switch it starts (the transition from the scene pair, the
    /// incoming scene or the workspace) goes to the compositors.
    fn set_program(&mut self, program: Option<Id>, workspace: &Workspace) {
        let previous = self.program;
        let incoming = program.and_then(|b| workspace.scene(b));
        // coming from a particular scene, that pair's own first
        let pair = previous.zip(incoming).and_then(|(a, s)| s.transitions_from.get(&a).copied());
        let settings = self
            .next_transition
            .take()
            .or(pair)
            .or_else(|| incoming.and_then(|s| s.transition))
            .unwrap_or(workspace.transition);
        let kind = match (previous, program) {
            (Some(a), Some(b)) if a != b => settings.kind,
            _ => TransitionKind::Cut,
        };
        let now = Instant::now();
        let switch = match (previous, program) {
            (Some(from), Some(to)) if kind != TransitionKind::Cut => {
                Some(crate::show::Switch { from, to, settings: TransitionSettings { kind, ..settings }, started: now })
            }
            _ => None,
        };
        crate::show::set_switch(switch);
        self.switch = switch.map(|s| Under {
            from: s.from,
            over: (kind == TransitionKind::Scene)
                .then(|| incoming.and_then(|s| s.transition_scene).or(workspace.show.transition_scene))
                .flatten(),
            ends: now + Duration::from_millis(settings.duration_ms as u64),
        });
        self.program = program;
    }

    /// The scenes drawn for outputs alone (the virtual camera's one scene): (scene, canvas).
    fn scene_targets(&self) -> Vec<(Id, Id)> {
        let Some(ws) = &self.workspace else { return Vec::new() };
        let mut out: Vec<(Id, Id)> = ws
            .canvases
            .iter()
            .flat_map(|c| crate::program::scene_targets(c.id).into_iter().map(|t| (t.scene, t.canvas)))
            .collect();
        out.sort();
        out
    }

    /// Works out which sources are on air and showing, and tells their runtimes what changed:
    /// media plays (or stops) on air, a web page is open while it shows, a timer autostarts on
    /// air.
    fn sync_on_air(&mut self) {
        let Some(ws) = self.workspace.clone() else { return };
        let mut scenes: Vec<Id> = ws.program.into_iter().chain(ws.show.overlays.iter().copied()).collect();
        if let Some(s) = &self.switch {
            scenes.push(s.from);
            scenes.extend(s.over);
        }
        scenes.extend(self.targets.iter().map(|(scene, _)| *scene));
        scenes.sort();
        scenes.dedup();
        let mut active = HashSet::new();
        for scene in scenes {
            let Some(scene) = ws.scene(scene) else { continue };
            for object in scene.flatten().objects {
                if let Some(source) = object.source
                    && object.layouts.values().any(|l| l.visible)
                {
                    active.insert(source);
                }
            }
        }
        #[cfg(any(target_os = "macos", windows))]
        let showing: HashSet<Id> = active.union(&self.held).copied().chain(crate::show::shown()).collect();
        for (id, rt) in self.runtimes.iter_mut() {
            match rt {
                Runtime::Timer(on_air) => on_air.store(active.contains(id), Ordering::Relaxed),
                #[cfg(target_os = "macos")]
                Runtime::Media { player, active: was, .. } => {
                    let now = active.contains(id);
                    if now != *was {
                        if now {
                            player.activate();
                        } else {
                            player.deactivate();
                        }
                        *was = now;
                    }
                }
                #[cfg(any(target_os = "macos", windows))]
                Runtime::Avatar(avatar) => avatar.set_shown(showing.contains(id)),
                #[cfg(target_os = "macos")]
                Runtime::Ffmpeg { media, active: was } => {
                    let now = active.contains(id);
                    if now != *was {
                        if now {
                            media.activate();
                        } else {
                            media.deactivate();
                        }
                        *was = now;
                    }
                }
                #[cfg(any(target_os = "macos", windows))]
                Runtime::Web { page, shown } => {
                    let now = showing.contains(id);
                    if now != *shown {
                        if now {
                            page.show();
                        } else {
                            page.hide();
                        }
                        *shown = now;
                    }
                }
                _ => {}
            }
        }
        self.active = active;
    }

    // ------------------------------------------------------------ sources' runtimes

    fn add_source(&mut self, source: &Source) {
        let need = need(source);
        self.make_runtime(source, &need);
        self.needs.insert(source.id, need);
    }

    /// A source's settings changed: its runtime follows them, or a new one is made when it needs
    /// another. Whether the mixer should look again.
    fn change_source(&mut self, source: &Source) -> bool {
        let need = need(source);
        if self.needs.get(&source.id) != Some(&need) {
            self.let_go(source.id);
            self.add_source(source);
            return true;
        }
        let s = &source.settings;
        match self.runtimes.get_mut(&source.id) {
            Some(Runtime::Timer(_)) => self.timers.configure(source.id, s),
            #[cfg(target_os = "macos")]
            Some(Runtime::Media { player, active, playlist }) => match playlist {
                Some(p) => {
                    if p.order.follow(s)
                        && let Some(file) = p.order.file()
                    {
                        player.update(playlist_file(file), *active);
                        p.started = Instant::now();
                    }
                }
                None => player.update(crate::media_player::Settings::from_json(s), *active),
            },
            #[cfg(any(target_os = "macos", windows))]
            Some(Runtime::Web { page, .. }) => page.set_options(page_options(s)),
            #[cfg(any(target_os = "macos", windows))]
            Some(Runtime::Avatar(avatar)) => avatar.set_settings(s),
            #[cfg(target_os = "macos")]
            Some(Runtime::Ffmpeg { media, .. }) => media.update(crate::ffmpeg_media::Settings::from_json(s)),
            _ => {}
        }
        matches!(need, Need::Device(_))
    }

    fn remove_source(&mut self, id: Id) {
        self.let_go(id);
        self.held.remove(&id);
        self.active.remove(&id);
        self.silent_logged.remove(&id);
    }

    /// Drops a source's runtime, the mixer letting go of its feed first (the runtime's feed goes
    /// with it; the mixer takes the new one when it next looks).
    fn let_go(&mut self, id: Id) {
        if let Some(a) = self.audio.as_mut() {
            a.release(id);
        }
        self.drop_runtime(id);
        self.needs.remove(&id);
    }

    fn drop_runtime(&mut self, id: Id) {
        match self.runtimes.remove(&id) {
            Some(Runtime::Timer(_)) => self.timers.remove(id),
            Some(Runtime::Link) => crate::audio_host::feeds_itself_no_more(id),
            // each stops on its own thread and leaves the mixer as it drops
            Some(rt) => drop(rt),
            None => {}
        }
    }

    /// Makes the runtime `need` says (off air: [`Self::sync_on_air`] puts it on).
    fn make_runtime(&mut self, source: &Source, need: &Need) {
        let id = source.id;
        let s = &source.settings;
        let rt = match need {
            Need::Nothing | Need::Device(_) => return,
            Need::Missing(what) => {
                if self.missing_logged.insert(*what) {
                    tracing::info!(what, kind = source.kind, "no runtime for this kind of source without libobs yet");
                }
                return;
            }
            Need::Timer => {
                let on_air = Arc::new(AtomicBool::new(false));
                self.timers.add(id, s, Some(Box::new(TimerOnAir(on_air.clone()))));
                Runtime::Timer(on_air)
            }
            #[cfg(target_os = "macos")]
            Need::Media => {
                let playlist = crate::playlists::Order::of(s).map(|order| Playlist { order, started: Instant::now() });
                let settings = match playlist.as_ref().and_then(|p| p.order.file()) {
                    Some(file) => playlist_file(file),
                    None => crate::media_player::Settings::from_json(s),
                };
                Runtime::Media {
                    player: crate::media_player::MediaPlayer::new(Some(id), settings, self.sample_rate, false, None),
                    active: false,
                    playlist,
                }
            }
            #[cfg(target_os = "macos")]
            Need::Soundboard => {
                let settings = crate::media_player::Settings {
                    path: String::new(),
                    looping: false,
                    restart_on_activate: false,
                    clear_on_end: true,
                };
                Runtime::Soundboard {
                    player: crate::media_player::MediaPlayer::new(Some(id), settings, self.sample_rate, false, None),
                    playing: Mutex::new((String::new(), false)),
                }
            }
            #[cfg(any(target_os = "macos", windows))]
            Need::Web => Runtime::Web { page: crate::web::WebPage::new(id, page_options(s), None, None), shown: false },
            Need::Ndi(name) => Runtime::Ndi { _receiver: crate::ndi::Receiver::start(id, name, None) },
            #[cfg(target_os = "macos")]
            Need::SystemSound(app) => Runtime::SystemSound {
                _sound: crate::system_audio::SystemSound::new(id, crate::system_audio::Settings { app: app.clone() }),
            },
            Need::Voice => Runtime::Voice { _voice: crate::piair_voice::Voice::start(id, None) },
            Need::Link => {
                crate::audio_host::feed_slot(id);
                Runtime::Link
            }
            #[cfg(target_os = "macos")]
            Need::Phone => Runtime::Phone(crate::phone_run::PhoneCamera::new(id)),
            #[cfg(any(target_os = "macos", windows))]
            Need::Avatar => Runtime::Avatar(crate::avatar_run::AvatarRuntime::new(id, s)),
            #[cfg(target_os = "macos")]
            Need::Ffmpeg => Runtime::Ffmpeg {
                media: crate::ffmpeg_media::FfmpegMedia::new(
                    id,
                    crate::ffmpeg_media::Settings::from_json(s),
                    self.sample_rate,
                ),
                active: false,
            },
            #[cfg(target_os = "macos")]
            Need::Dslr => Runtime::Dslr { _live: crate::dslr::LiveView::start(id) },
            #[allow(unreachable_patterns)]
            _ => return,
        };
        self.runtimes.insert(id, rt);
    }

    // ------------------------------------------------------------ the mixer

    /// The direct monitor as the audio engine takes it.
    fn monitor_spec(&self) -> Option<(String, lightair_audio::NodeId)> {
        self.monitor.clone().map(|(d, program)| (d, if program { lightair_audio::PROGRAM } else { crate::audio::CUE }))
    }

    /// Brings the audio engine up to date: opens the interfaces and microphones tracks use,
    /// takes every other track's source from the runtime feeding it, lets go of what no track
    /// uses, and plays the workspace's mixer.
    fn sync_audio(&mut self, workspace: &Workspace) {
        if !crate::audio_host::mixer_on() || (workspace.tracks.is_empty() && self.audio.is_none()) {
            return;
        }
        if self.audio.is_none() {
            crate::plugins::set_sample_rate(self.sample_rate);
            let block = self.audio_block.unwrap_or(crate::audio_host::BLOCK);
            self.audio = AudioHost::start(self.sample_rate, block);
        }
        let monitor = self.monitor_spec();
        let Some(host) = self.audio.as_mut() else { return };
        if let Err(e) = host.set_headphones(monitor) {
            tracing::warn!(%e, "direct monitoring unavailable");
        }
        let extras: Vec<(lightair_audio::NodeId, String)> = workspace
            .audio
            .mixes
            .iter()
            .filter_map(|m| m.device.clone().map(|d| (crate::audio::node(m.id), d)))
            .collect();
        for e in host.set_extra_outputs(extras) {
            tracing::warn!(error = %e, "a mix's output device didn't open");
        }
        let wanted: HashSet<Id> = workspace.tracks.iter().map(|t| t.source).collect();
        let needs = &self.needs;
        let device = |id: &Id| match needs.get(id) {
            Some(Need::Device(d)) => Some(d.clone()),
            _ => None,
        };
        let stale: Vec<Id> = host.captured().filter(|i| !wanted.contains(i) || device(i).is_some()).collect();
        for source in stale {
            host.release(source);
        }
        let stale: Vec<Id> = host.open_devices().filter(|i| !wanted.contains(i) || device(i).is_none()).collect();
        for source in stale {
            host.close_device(source);
        }
        for source in &wanted {
            match device(source) {
                Some(d) => {
                    if let Err(e) = host.open_device(*source, &d) {
                        tracing::warn!(%e, device = d, "couldn't open the audio input");
                    }
                }
                None => {
                    if !host.capture(*source) && self.silent_logged.insert(*source) {
                        tracing::debug!(%source, "a track's source doesn't feed the mixer without libobs");
                    }
                }
            }
        }
        host.set(crate::audio::spec(workspace));
        crate::plugins::retain(&crate::plugins::inserts(workspace));
        let cell = |id: Id| meters().get(id).unwrap_or_else(|| meters().insert(id));
        let mut cells: Vec<_> = workspace.tracks.iter().map(|t| (crate::audio::node(t.id), cell(t.id))).collect();
        // buses, mixes and the master show in the mixer too
        for id in workspace.audio.buses.iter().map(|b| b.id).chain(workspace.audio.mixes.iter().map(|m| m.id)) {
            cells.push((crate::audio::node(id), cell(id)));
        }
        cells.push((lightair_audio::PROGRAM, cell(lightair_model::MASTER_ID)));
        host.set_meter_cells(cells);
    }

    /// Sets the audio engine's block size (frames): smaller is lower latency, larger a lighter
    /// load. Restarts the engine if it's running (not while multitrack files record: false).
    pub fn set_audio_block(&mut self, frames: usize, workspace: &Workspace) -> bool {
        if self.audio_block == Some(frames) {
            return true;
        }
        if crate::audio_host::STEMS_RUNNING.load(Ordering::Relaxed) {
            return false;
        }
        self.audio_block = Some(frames);
        if self.audio.as_ref().is_some_and(|a| a.block() != frames) {
            self.audio = None;
            self.sync_audio(workspace);
        }
        true
    }

    /// The user answered macOS's microphone prompt ([`crate::access::changed`]): the mixer opens
    /// the inputs it held back.
    pub fn access_answered(&mut self, workspace: &Workspace) {
        self.sync_audio(workspace);
    }

    /// Plays the mixer again (a plugin insert finished loading: [`crate::plugins::changed`]).
    pub fn refresh_audio(&mut self, workspace: &Workspace) {
        self.sync_audio(workspace);
    }

    /// Starts recording multitrack files into `folder`, named "<base> - <strip>.wav": the
    /// tracks and buses marked Record (every track when none is), and the program mix when
    /// `program` (audio-only recordings). Returns the files and whose sound each is.
    pub fn start_multitrack(
        &mut self,
        workspace: &Workspace,
        folder: &std::path::Path,
        base: &str,
        program: bool,
    ) -> Result<Vec<(std::path::PathBuf, String)>, String> {
        let settings = &workspace.outputs.recording;
        let host = self.audio.as_mut().ok_or("the audio engine isn't running")?;
        let tap = match settings.multitrack_point {
            lightair_model::MultitrackPoint::Raw => lightair_audio::Tap::Input,
            lightair_model::MultitrackPoint::PreFader => lightair_audio::Tap::PreFader,
            lightair_model::MultitrackPoint::PostFader => lightair_audio::Tap::PostFader,
        };
        let float = settings.multitrack_format == lightair_model::MultitrackFormat::Float32;
        let marked = workspace.tracks.iter().any(|t| t.record) || workspace.audio.buses.iter().any(|b| b.record);
        let mut strips: Vec<(lightair_audio::NodeId, String, u16)> = workspace
            .tracks
            .iter()
            .filter(|t| t.record || !marked)
            .map(|t| {
                // a mono channel records as mono (before the pan it's the same on both sides)
                let ch = if matches!(t.channels, lightair_model::TrackChannels::Mono(_))
                    && tap != lightair_audio::Tap::PostFader
                {
                    1
                } else {
                    2
                };
                (crate::audio::node(t.id), t.name.clone(), ch)
            })
            .collect();
        strips.extend(
            workspace.audio.buses.iter().filter(|b| b.record).map(|b| (crate::audio::node(b.id), b.name.clone(), 2)),
        );
        if program {
            strips.push((lightair_audio::PROGRAM, "Program".into(), 2));
        }
        if strips.is_empty() {
            return Err("no tracks to record".into());
        }
        std::fs::create_dir_all(folder).map_err(|e| e.to_string())?;
        let clean = lightair_model::file_safe;
        let mut used = HashSet::new();
        let mut files = Vec::new();
        let specs = strips
            .into_iter()
            .map(|(id, name, channels)| {
                let mut file = format!("{base} - {}.wav", clean(&name));
                let mut n = 2;
                while !used.insert(file.clone()) {
                    file = format!("{base} - {} {n}.wav", clean(&name));
                    n += 1;
                }
                // the file, and whose sound it is (its name as it was, not read back from the file's)
                files.push((folder.join(&file), name));
                crate::audio_host::TrackFileSpec { id, path: folder.join(file), channels, tap, float }
            })
            .collect::<Vec<_>>();
        host.start_multitrack(specs)?;
        Ok(files)
    }

    /// Stops the multitrack files; what they hold.
    pub fn stop_multitrack(&mut self) -> Option<MultitrackReport> {
        self.audio.as_mut()?.stop_multitrack()
    }

    pub fn pause_multitrack(&mut self, pause: bool) {
        if let Some(h) = self.audio.as_mut() {
            h.pause_multitrack(pause);
        }
    }

    /// Monitoring straight from the audio engine to an output device (`None`: none).
    /// `program`: hear the stream mix rather than the cue.
    pub fn set_headphones(&mut self, device: Option<String>, program: bool) {
        // without libobs nothing else plays the cue: the monitoring device chosen, when no
        // device is given for direct monitoring
        self.monitor_follows = device.is_none();
        let device = device.unwrap_or_else(|| crate::monitoring::current().1);
        self.monitor = Some((if device == "default" { String::new() } else { device }, program));
        let monitor = self.monitor_spec();
        if let Some(host) = self.audio.as_mut()
            && let Err(e) = host.set_headphones(monitor)
        {
            tracing::warn!(%e, "direct monitoring unavailable");
        }
    }

    /// A monitor the caller reads (tests): stereo, interleaved.
    pub fn audio_test_monitor(&mut self, program: bool) -> Option<rtrb::Consumer<f32>> {
        let src = if program { lightair_audio::PROGRAM } else { crate::audio::CUE };
        Some(self.audio.as_mut()?.test_monitor(src))
    }

    /// A source the caller feeds (tests): `channels` interleaved frames, like an interface.
    pub fn audio_test_input(&mut self, source: Id, channels: u16) -> Option<rtrb::Producer<f32>> {
        Some(self.audio.as_mut()?.test_input(source, channels))
    }

    /// A track's sound to listen to (the co-host hears it): see [`AudioHost::listen`]. `None`
    /// when the audio engine isn't running.
    pub fn listen_track(
        &mut self,
        track: Id,
        tap: lightair_audio::Tap,
        muted: Arc<AtomicBool>,
    ) -> Option<rtrb::Consumer<i16>> {
        Some(self.audio.as_mut()?.listen(crate::audio::node(track), tap, muted))
    }

    /// The program's sound as it goes out (the Audio visualiser): see [`AudioHost::listen`].
    pub fn listen_program(&mut self) -> Option<rtrb::Consumer<i16>> {
        let never = Arc::new(AtomicBool::new(false));
        Some(self.audio.as_mut()?.listen(lightair_audio::PROGRAM, lightair_audio::Tap::PostFader, never))
    }

    /// A track's, bus's or mix's meter from the audio engine.
    pub fn audio_meter(&mut self, id: Id) -> Option<Arc<lightair_audio::meter::Meter>> {
        Some(self.audio.as_mut()?.meter(crate::audio::node(id)))
    }

    /// The program's (master's) meter.
    pub fn program_meter(&mut self) -> Option<Arc<lightair_audio::meter::Meter>> {
        Some(self.audio.as_mut()?.meter(lightair_audio::PROGRAM))
    }

    // ------------------------------------------------------------ media, web, timers

    /// Playback status of a media source (or the soundboard), or `None` if it isn't one.
    pub fn media_status(&self, source: Id) -> Option<MediaStatus> {
        #[cfg(target_os = "macos")]
        if let Some(Runtime::Media { player, .. } | Runtime::Soundboard { player, .. }) = self.runtimes.get(&source) {
            return Some(MediaStatus {
                state: crate::media::MediaState::from_code(player.state()),
                time_ms: player.time_ms(),
                duration_ms: player.duration_ms(),
            });
        }
        #[cfg(target_os = "macos")]
        if let Some(Runtime::Ffmpeg { media, .. }) = self.runtimes.get(&source) {
            return Some(MediaStatus {
                state: crate::media::MediaState::from_code(media.state()),
                time_ms: media.time_ms(),
                duration_ms: media.duration_ms(),
            });
        }
        let _ = source;
        None
    }

    pub fn media_action(&self, source: Id, action: MediaAction) {
        #[cfg(target_os = "macos")]
        if let Some(Runtime::Media { player, .. } | Runtime::Soundboard { player, .. }) = self.runtimes.get(&source) {
            match action {
                MediaAction::Play => player.play_pause(false),
                MediaAction::Pause => player.play_pause(true),
                MediaAction::Restart => player.restart(),
                MediaAction::Stop => player.stop(),
                MediaAction::Seek(ms) => player.set_time_ms(ms),
            }
        }
        #[cfg(target_os = "macos")]
        if let Some(Runtime::Ffmpeg { media, .. }) = self.runtimes.get(&source) {
            match action {
                MediaAction::Play => media.play_pause(false),
                MediaAction::Pause => media.play_pause(true),
                MediaAction::Restart => media.restart(),
                MediaAction::Stop => media.stop(),
                MediaAction::Seek(ms) => media.set_time_ms(ms),
            }
        }
        let _ = (source, action);
    }

    /// Restyles a web page source's page now, with custom CSS not yet in its settings.
    pub fn web_restyle(&self, source: Id, css: String) {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(Runtime::Web { page, .. }) = self.runtimes.get(&source) {
            page.restyle(css);
        }
        #[cfg(not(any(target_os = "macos", windows)))]
        let _ = (source, css);
    }

    /// Loads a web page source's page again.
    pub fn web_reload(&self, source: Id) {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(Runtime::Web { page, .. }) = self.runtimes.get(&source) {
            page.reload();
        }
        let _ = source;
    }

    pub fn timer_action(&self, source: Id, action: TimerAction) {
        self.timers.action(source, action);
    }

    pub fn timer_status(&self, source: Id) -> Option<TimerStatus> {
        self.timers.status(source)
    }

    /// The text a timer is showing.
    pub fn timer_text(&self, source: Id) -> Option<String> {
        self.timers.text(source)
    }

    /// Countdowns that ended since the last call (their end actions are the app's to run).
    pub fn take_ended_timers(&self) -> Vec<Id> {
        self.timers.take_ended()
    }

    /// Plays `file` on the soundboard `source` from the start (`looped`: over and over); `None`
    /// stops it. The pad's `volume` (0..1) isn't applied yet: the track's fader is.
    pub fn soundboard_play(&self, source: Id, file: Option<(&str, bool, f32)>) {
        #[cfg(target_os = "macos")]
        if let Some(Runtime::Soundboard { player, playing }) = self.runtimes.get(&source) {
            match file {
                Some((path, looped, _volume)) => {
                    let mut now = playing.lock();
                    let new_file = now.0 != path;
                    if new_file || now.1 != looped {
                        *now = (path.to_string(), looped);
                        let settings = crate::media_player::Settings {
                            path: path.to_string(),
                            looping: looped,
                            restart_on_activate: false,
                            clear_on_end: true,
                        };
                        // a new file plays from the start at once
                        player.update(settings, true);
                    }
                    if !new_file {
                        player.restart();
                    }
                }
                None => player.stop(),
            }
            return;
        }
        let _ = (source, file);
    }

    /// Whether the soundboard `source` is playing now.
    pub fn soundboard_playing(&self, source: Id) -> bool {
        #[cfg(target_os = "macos")]
        if let Some(Runtime::Soundboard { player, .. }) = self.runtimes.get(&source) {
            return player.state() == crate::media_player::state::PLAYING;
        }
        let _ = source;
        false
    }

    /// Moves a playlist `by` files and plays from there.
    pub fn playlist_skip(&mut self, source: Id, by: i64) {
        #[cfg(target_os = "macos")]
        if let Some(Runtime::Media { player, active, playlist: Some(p) }) = self.runtimes.get_mut(&source) {
            p.order.skip(by);
            if let Some(file) = p.order.file() {
                player.update(playlist_file(file), *active);
                player.restart();
                p.started = Instant::now();
            }
        }
        let _ = (source, by);
    }

    /// A playlist's place: (from 1, how many, the file playing).
    pub fn playlist_now(&self, source: Id) -> Option<(usize, usize, String)> {
        #[cfg(target_os = "macos")]
        if let Some(Runtime::Media { playlist: Some(p), .. }) = self.runtimes.get(&source) {
            return p.order.now();
        }
        let _ = source;
        None
    }

    // ------------------------------------------------------------ avatars and face effects

    /// The screens and cameras a Studio take records: those visible in the program, each once.
    pub fn studio_take_sources(&self, workspace: &Workspace) -> Vec<(lightair_model::Source, Id)> {
        let Some(scene) = workspace.program.and_then(|p| workspace.scene(p)) else { return Vec::new() };
        let canvas = workspace.primary_canvas().id;
        let mut out: Vec<(lightair_model::Source, Id)> = Vec::new();
        for object in scene.flatten().objects {
            let visible = object.layouts.get(&canvas).is_none_or(|l| l.visible);
            let Some(source) = object.source.and_then(|i| workspace.sources.get(&i)) else { continue };
            if !visible
                || !matches!(source.kind.as_str(), lightair_model::kinds::SCREEN | lightair_model::kinds::CAMERA)
                || out.iter().any(|(s, _)| s.id == source.id)
            {
                continue;
            }
            out.push(((**source).clone(), source.id));
        }
        out
    }

    /// An avatar source's status (`None`: avatars don't run without libobs yet).
    pub fn avatar_status(&self, source: Id) -> Option<AvatarStatus> {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(Runtime::Avatar(avatar)) = self.runtimes.get(&source) {
            return Some(avatar.status());
        }
        let avatar =
            self.workspace.as_ref().and_then(|w| w.sources.get(&source)).is_some_and(|s| s.kind == crate::avatar::KIND);
        avatar.then(|| avatar_status_of(&source))
    }

    /// A model drawn on its own for a library picture (`None`: not without libobs yet).
    pub fn avatar_snapshot_source(&self, _file: &str, _framing: &str) -> Option<Id> {
        not_yet("avatar_snapshot_source");
        None
    }

    /// A PNGtuber's push to talk held, or let go.
    pub fn avatar_push_to_talk(&self, source: Id, held: bool) {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(Runtime::Avatar(avatar)) = self.runtimes.get(&source) {
            avatar.set_push_to_talk(held);
        }
        let _ = (source, held);
    }

    /// Where an avatar's anchor is in its picture at rest (pixels), once the compositor draws its
    /// model. `size`: as it would be at that size of the picture instead.
    pub fn avatar_anchor_rest(&self, source: Id, key: &str, size: Option<(u32, u32)>) -> Option<Spot> {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(Runtime::Avatar(avatar)) = self.runtimes.get(&source) {
            return avatar.anchor_rest(key, size);
        }
        let _ = (source, key, size);
        None
    }

    /// Takes the face an avatar sees now as its neutral one: the calibration's JSON (`None`: no
    /// face seen).
    pub fn avatar_calibrate(&self, source: Id) -> Option<String> {
        #[cfg(any(target_os = "macos", windows))]
        if let Some(Runtime::Avatar(avatar)) = self.runtimes.get(&source) {
            return avatar.calibrate();
        }
        let _ = source;
        None
    }

    /// Whether the source's face effects see a face right now (`None`: none looking).
    pub fn face_present(&self, source: Id) -> Option<bool> {
        #[cfg(any(target_os = "macos", windows))]
        return crate::bridge::face_seen(source);
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let _ = source;
            None
        }
    }

    /// Whether the source's effects see a person right now (`None`: none looking).
    pub fn person_present(&self, source: Id) -> Option<bool> {
        #[cfg(any(target_os = "macos", windows))]
        return crate::bridge::person_seen(source);
        #[cfg(not(any(target_os = "macos", windows)))]
        {
            let _ = source;
            None
        }
    }

    /// Turns one of a source's effects on or off live, leaving the document alone
    /// (`lightair_model::catalog::STUDIO_ID`: its looks): the compositor draws it so.
    pub fn set_filter_live(&self, source: Id, filter: Id, enabled: bool) {
        crate::show::set_live(source, filter, enabled);
    }

    // ------------------------------------------------------------ sources

    /// Starts sources as if they were on screen (web pages open) without putting them on air,
    /// until `release_sources`.
    pub fn hold_sources(&mut self, sources: &[Id]) {
        self.held.extend(sources.iter().copied());
        self.sync_on_air();
    }

    pub fn release_sources(&mut self) {
        if !self.held.is_empty() {
            self.held.clear();
            self.sync_on_air();
        }
    }

    /// The choices a source offers for a list setting, by the setting's name as libobs's source
    /// called it (`crate::kinds::libobs_key`): a screen's displays, windows or apps, a camera's
    /// devices, a microphone's (or a loopback device's) inputs, the apps whose sound can be
    /// taken. (label, value).
    pub fn source_options(&self, source: Id, property: &str) -> Vec<(String, serde_json::Value)> {
        use serde_json::Value;
        let kind = self.workspace.as_ref().and_then(|w| w.sources.get(&source)).map(|s| s.kind.clone());
        let screen = |mode: i64, key: &str| crate::displays::screen_options(mode, key);
        match (kind.as_deref(), property) {
            (Some(kinds::SCREEN), "display_uuid" | "monitor_id") => screen(kinds::DISPLAY, "display"),
            (Some(kinds::SCREEN), "window") => screen(kinds::WINDOW, "window"),
            (Some(kinds::SCREEN), "application") => screen(kinds::APP, "app"),
            (Some(kinds::APP_AUDIO), "application" | "window") => screen(kinds::APP, "app"),
            (Some(kinds::CAMERA), "device" | "video_device_id") => {
                crate::kinds::cameras().into_iter().map(|(name, id)| (name, Value::from(id))).collect()
            }
            (Some(kinds::MIC | kinds::DESKTOP_AUDIO), "device_id" | "device") => {
                std::iter::once(("Default".to_string(), Value::from("default")))
                    .chain(crate::devices::input_devices().into_iter().map(|d| (d.name, Value::from(d.id))))
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// Every source's pixel size that's known (media files once open, web pages).
    pub fn source_sizes(&self) -> HashMap<Id, (u32, u32)> {
        self.runtimes.keys().filter_map(|id| self.source_size(*id).map(|s| (*id, s))).collect()
    }

    /// A source's pixel size, once it has one: a media file's picture (turned upright), a web
    /// page's size. `None` for anything else.
    pub fn source_size(&self, source: Id) -> Option<(u32, u32)> {
        match self.runtimes.get(&source)? {
            #[cfg(target_os = "macos")]
            Runtime::Media { player, .. } => {
                let ((w, h), rotation) = player.natural();
                let (w, h) = if rotation % 180 == 90 { (h, w) } else { (w, h) };
                (w > 0 && h > 0).then_some((w, h))
            }
            #[cfg(target_os = "macos")]
            Runtime::Ffmpeg { media, .. } => {
                let (w, h) = media.size();
                (w > 0 && h > 0).then_some((w, h))
            }
            #[cfg(any(target_os = "macos", windows))]
            Runtime::Web { page, .. } => {
                let o = page.options();
                Some((o.width, o.height))
            }
            _ => None,
        }
    }

    /// Shows an object whole while it's being cropped (crop mode), or `None` to end it: the
    /// compositor draws the rest of its picture as a faint ghost around the crop.
    pub fn set_crop_preview(&mut self, target: Option<(Id, Id, Id)>, _workspace: &Workspace) {
        self.crop_preview = target;
        crate::show::set_crop_view(target, false);
    }

    /// Fades the crop view's ghost out (before `set_crop_preview(None)`, so leaving crop view
    /// doesn't pop).
    pub fn fade_crop_preview(&mut self, _workspace: &Workspace) {
        crate::show::set_crop_view(self.crop_preview, true);
    }

    /// Whether a source is running: on air (the program, overlays, a switch, a scene drawn for
    /// an output), or a sound-only source a track takes.
    pub fn source_active(&self, source: Id) -> bool {
        if self.active.contains(&source) {
            return true;
        }
        let sound_only = matches!(
            self.needs.get(&source),
            Some(Need::Device(_) | Need::SystemSound(_) | Need::Voice | Need::Soundboard)
        );
        sound_only && self.workspace.as_ref().is_some_and(|ws| ws.tracks.iter().any(|t| t.source == source))
    }

    /// The source, for a preview or an audio tap off the engine thread: its id while the
    /// document has it.
    pub fn source_ref(&self, source: Id) -> Option<Id> {
        self.workspace.as_ref()?.sources.contains_key(&source).then_some(source)
    }

    /// A document scene in a canvas, while both exist: (scene, canvas).
    pub fn scene_source(&self, scene: Id, canvas: Id) -> Option<(Id, Id)> {
        let ws = self.workspace.as_ref()?;
        (ws.scene(scene).is_some() && ws.canvas(canvas).is_some()).then_some((scene, canvas))
    }

    /// That scene's picture, e.g. for Studio mode's preview of a scene that isn't on air: the
    /// same (scene, canvas), which the compositor draws.
    pub fn scene_source_ref(&self, scene: Id, canvas: Id) -> Option<(Id, Id)> {
        self.scene_source(scene, canvas)
    }

    /// A canvas, for its preview: its id while the document has it.
    pub fn canvas_ref(&self, canvas: Id) -> Option<Id> {
        self.workspace.as_ref()?.canvas(canvas).map(|c| c.id)
    }

    /// The canvas the outputs record or stream: its id while the document has it (the outputs
    /// take its compositor's frames).
    pub fn canvas_video(&self, canvas: Id) -> Option<Id> {
        self.canvas_ref(canvas)
    }

    /// What the document holds as the libobs engine counted it: (sources, scenes in each canvas,
    /// objects with a source in each canvas). For tests and diagnostics.
    pub fn counts(&self) -> (usize, usize, usize) {
        let Some(ws) = &self.workspace else { return (0, 0, 0) };
        let canvases = ws.canvases.len();
        let items: usize =
            ws.scenes.iter().map(|s| s.flatten().objects.iter().filter(|o| o.source.is_some()).count()).sum();
        (ws.sources.len(), ws.scenes.len() * canvases, items * canvases)
    }

    /// Per scene and canvas, the objects bottom to top with source, position, size, rotation
    /// and visibility, from the document as applied. For tests and diagnostics.
    pub fn describe(&self) -> Vec<String> {
        let Some(ws) = &self.workspace else { return Vec::new() };
        let mut keys: Vec<(Id, Id)> =
            ws.scenes.iter().flat_map(|s| ws.canvases.iter().map(move |c| (s.id, c.id))).collect();
        keys.sort();
        let mut out = Vec::new();
        for (scene, canvas) in keys {
            out.push(format!(
                "scene {scene} format {canvas}{}",
                if self.program == Some(scene) { " (program)" } else { "" }
            ));
            let Some(s) = ws.scene(scene) else { continue };
            for object in s.flatten().objects {
                let Some(source) = object.source else { continue };
                match object.layouts.get(&canvas) {
                    Some(l) => out.push(format!(
                        "  {source} pos=({:.1},{:.1}) bounds=({:.1},{:.1}) rot={:.1} visible={}",
                        l.x, l.y, l.width, l.height, l.rotation, l.visible
                    )),
                    None => out.push(format!("  {source} visible=false")),
                }
            }
        }
        out
    }
}
