//! The reconciler: makes libobs match the Lightair document (docs/04-engine-libobs.md,
//! "Reconciler"). It consumes patches from `lightair_core::diff`, runs on the engine thread,
//! and is idempotent: applying `diff(empty, project)` to an empty engine gives the same libobs
//! state as any sequence of edits ending at `project`.
//!
//! libobs objects are named by their Lightair id, so user-facing names never collide.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;

use libobs_sys as sys;
use lightair_core::catalog::lower;
use lightair_core::{
    Effect, Fit, Format, Id, Layer, LayerPatch, Layout, Patch, Project, Track, TransitionKind, TransitionSettings,
    diff_layers,
};
use parking_lot::Mutex;

use crate::timers::{TIMER_KIND, TIMER_SOURCE, TimerAction, TimerStatus, Timers, on_timers_tick, tick_param};
use crate::transition::{FormatMove, MoveAnim, Tween, item_fade, match_layer};

use crate::meters::{MeterCell, db_to_mul, meters};
use crate::obs::{Canvas, Data, Scene, Source, vec2};
use std::sync::Arc;

/// libobs state owned on behalf of the document.
///
/// Every document scene exists once per format: one libobs scene in that format's canvas,
/// all sharing the same input sources (docs/07-scenes-layers-canvas.md, "Formats").
#[derive(Default)]
pub struct Reconciler {
    inputs: HashMap<Id, Source>,
    /// Input effect chains (Camera Studio, colour correction, …).
    input_effects: HashMap<Id, Chain>,
    /// Canvases for additional formats. The primary format uses libobs's main canvas.
    canvases: HashMap<Id, Canvas>,
    /// (scene, format) → libobs scene.
    scenes: HashMap<(Id, Id), Scene>,
    /// Scene items are owned by their scene; (scene, format, layer) → item.
    items: HashMap<(Id, Id, Id), ItemPtr>,
    /// Shaped layers: the item above is a one-item group; this is the input's item inside it
    /// (carrying the crop) and the shape mask on the group.
    masks: HashMap<(Id, Id, Id), Masked>,
    /// The layer being cropped (scene, format, layer): shown whole and unshaped meanwhile.
    crop_preview: Option<(Id, Id, Id)>,
    /// Leaving crop view: the ghost is fading out.
    crop_fading: bool,
    formats: Vec<Id>,
    primary_format: Option<Id>,
    program: Option<Id>,
    tracks: HashMap<Id, TrackState>,
    /// The audio engine playing the mixer (docs/plans/audio-engine.md); started with the first
    /// track. `LIGHTAIR_LEGACY_AUDIO=1` keeps libobs mixing the tracks instead.
    audio: Option<crate::audio_host::AudioHost>,
    /// The direct monitor wanted: output device ("" default) and whether it plays the program
    /// (else the cue mix). Applied whenever the audio engine runs.
    monitor: Option<(String, bool)>,
    /// Frames per audio block (`None`: the default).
    audio_block: Option<usize>,
    /// Each format's output channel goes through its own transition.
    transitions: HashMap<Id, Source>,
    transition_settings: TransitionSettings,
    stinger: Option<lightair_core::Stinger>,
    /// The running Move, driven by a libobs tick callback.
    anim: Arc<Mutex<Option<MoveAnim>>>,
    tick_registered: bool,
    /// Inputs held "showing" off air (e.g. to learn their size), released with `release_inputs`.
    held: HashSet<Id>,
    /// Countdown/stopwatch/clock inputs, driven by their own tick callback.
    timers: Arc<Timers>,
    timers_tick: bool,
}

/// libobs state for one mixer track: its output channel, volume meter and effect filters.
struct TrackState {
    input: Id,
    /// Output channel (1..64) that keeps the track's source mixed when it isn't in a scene.
    channel: u32,
    volmeter: VolmeterPtr,
    meter: Arc<MeterCell>,
    filters: Vec<(Id, Source)>,
}

struct VolmeterPtr(*mut sys::obs_volmeter_t);
unsafe impl Send for VolmeterPtr {}

#[derive(Clone, Copy)]
struct ItemPtr(*mut sys::obs_sceneitem_t);
unsafe impl Send for ItemPtr {}

/// The inside of a layer drawn through Lightair's filters: the input's item in the group, the
/// shape mask, and the flip/3D transform (applied in that order).
pub(crate) struct Masked {
    pub inner: *mut sys::obs_sceneitem_t,
    pub filter: Source,
    pub transform: Option<Source>,
}
unsafe impl Send for Masked {}

impl Reconciler {
    /// Applies patches computed from two snapshots; `project` is the newer one.
    pub fn apply(&mut self, project: &Project, patches: &[Patch]) {
        self.primary_format = Some(project.primary_format().id);
        let transition_changed = self.transition_settings != project.transition || self.stinger != project.stinger;
        self.transition_settings = project.transition;
        self.stinger = project.stinger.clone();
        self.cleanup();
        // Formats are synced from the snapshot itself, so a rebuild against `empty_like()`
        // (which keeps the formats) still creates every canvas.
        let mut formats_added = false;
        for format in &project.formats {
            if !self.formats.contains(&format.id) {
                self.add_format(project, format);
                formats_added = true;
            }
        }
        for id in self.formats.clone() {
            if project.format(id).is_none() {
                self.remove_format(id);
            }
        }
        // Set transitions up when they change, not at the switch: a stinger has to load its
        // video before it can play.
        if transition_changed || formats_added {
            self.prepare_transitions();
        }
        for patch in patches {
            match patch {
                Patch::FormatAdded(_) | Patch::FormatRemoved(_) => {}
                Patch::FormatChanged(format) => self.resize_format(format),
                Patch::InputAdded(input) => {
                    let settings = data_from_json(&input.settings);
                    let timer = input.kind == TIMER_KIND;
                    let kind = if timer { TIMER_SOURCE } else { input.kind.as_str() };
                    match Source::create(kind, &input.id.to_string(), Some(&settings)) {
                        Some(src) => {
                            if timer {
                                self.timers.add(input.id, src.as_ptr(), &input.settings);
                                self.ensure_timers_tick();
                            }
                            let chain = unsafe { chain_build(src.as_ptr(), &lower(&input.effects)) };
                            self.input_effects.insert(input.id, chain);
                            self.inputs.insert(input.id, src);
                        }
                        None => tracing::warn!(kind = input.kind, name = input.name, "could not create input"),
                    }
                }
                Patch::InputChanged { before, after } => {
                    if let Some(src) = self.inputs.get(&after.id) {
                        if before.settings != after.settings {
                            src.update(&data_from_json(&after.settings));
                            if after.kind == TIMER_KIND {
                                self.timers.configure(after.id, &after.settings);
                            }
                        }
                        let chain = self.input_effects.entry(after.id).or_default();
                        unsafe { chain_update(src.as_ptr(), chain, &lower(&before.effects), &lower(&after.effects)) };
                    }
                }
                Patch::InputRemoved(id) => {
                    self.timers.remove(*id);
                    if self.held.remove(id)
                        && let Some(src) = self.inputs.get(id)
                    {
                        unsafe { sys::obs_source_dec_showing(src.as_ptr()) };
                    }
                    if let (Some(src), Some(mut chain)) = (self.inputs.get(id), self.input_effects.remove(id)) {
                        unsafe { chain_remove(src.as_ptr(), &mut chain) };
                    }
                    self.inputs.remove(id);
                }
                // libobs draws the flattened hierarchy: pictures only, in world terms (a parent
                // moving changes its children's world layouts, so they update too).
                Patch::SceneAdded(scene) => {
                    let scene = scene.flatten();
                    for format in self.formats.clone() {
                        self.add_scene_in(&scene, format);
                    }
                }
                Patch::SceneChanged { before, after } => {
                    let (before, after) = (&before.flatten(), &after.flatten());
                    for format in self.formats.clone() {
                        for lp in diff_layers(before, after) {
                            match lp {
                                LayerPatch::Added { layer, index } => self.add_layer(after.id, format, layer, index),
                                LayerPatch::Removed { layer } => {
                                    self.masks.remove(&(after.id, format, layer));
                                    if let Some(item) = self.items.remove(&(after.id, format, layer)) {
                                        unsafe { sys::obs_sceneitem_remove(item.0) };
                                    }
                                }
                                LayerPatch::Changed { layer } => {
                                    let index = after.layers.iter().position(|l| l.id == layer.id).unwrap_or(0);
                                    self.update_layer(after.id, format, layer, index)
                                }
                                LayerPatch::Order(order) => self.reorder(after.id, format, &order),
                            }
                        }
                    }
                }
                Patch::SceneRemoved(id) => {
                    if self.program == Some(*id) {
                        self.set_program(None, project);
                    }
                    self.items.retain(|(scene, _, _), _| scene != id);
                    self.masks.retain(|(scene, _, _), _| scene != id);
                    self.scenes.retain(|(scene, _), _| scene != id);
                }
                Patch::ProgramChanged(program) => self.set_program(*program, project),
                Patch::TrackAdded(track) => self.add_track(track),
                Patch::TrackChanged { before, after } => self.update_track(before, after),
                Patch::TrackRemoved(id) => self.remove_track(*id),
                Patch::TrackOrder(_) => {}
                // The audio engine takes the mixer from the project (docs/plans/audio-engine.md).
                Patch::AudioSetup => {}
            }
        }
        let audio_changed = patches.iter().any(|p| {
            matches!(
                p,
                Patch::TrackAdded(_)
                    | Patch::TrackChanged { .. }
                    | Patch::TrackRemoved(_)
                    | Patch::AudioSetup
                    | Patch::InputAdded(_)
                    | Patch::InputRemoved(_)
            ) || matches!(p, Patch::InputChanged { after, .. } if after.kind == crate::devices::DEVICE_KIND)
        });
        if audio_changed {
            self.sync_audio(project);
        }
    }

    /// Whether the audio engine plays the mixer (rather than libobs mixing the tracks).
    fn engine_audio() -> bool {
        std::env::var("LIGHTAIR_LEGACY_AUDIO").map_or(true, |v| v != "1")
    }

    /// Brings the audio engine up to date: captures every track's input, releases inputs no
    /// track uses, and plays the project's mixer.
    fn sync_audio(&mut self, project: &Project) {
        if !Self::engine_audio() || (project.tracks.is_empty() && self.audio.is_none()) {
            return;
        }
        if self.audio.is_none() {
            let mut info = sys::obs_audio_info::default();
            let rate = if unsafe { sys::obs_get_audio_info(&mut info) } { info.samples_per_sec } else { 48_000 };
            crate::plugins::set_sample_rate(rate);
            let block = self.audio_block.unwrap_or(crate::audio_host::BLOCK);
            self.audio = crate::audio_host::AudioHost::start(rate, block);
        }
        let Some(host) = self.audio.as_mut() else { return };
        let monitor = self.monitor.clone().map(|(d, program)| (d, if program { lightair_audio::PROGRAM } else { crate::audio::CUE }));
        if let Err(e) = host.set_monitor(monitor) {
            tracing::warn!(%e, "direct monitoring unavailable; libobs monitors the cue");
        }
        let wanted: HashSet<Id> = project.tracks.iter().map(|t| t.input).collect();
        let is_device = |id: &Id| project.inputs.get(id).is_some_and(|i| i.kind == crate::devices::DEVICE_KIND);
        let stale: Vec<Id> = host.captured().filter(|i| !wanted.contains(i) || is_device(i)).collect();
        for input in stale {
            host.release(input);
        }
        let stale: Vec<Id> = host.open_devices().filter(|i| !wanted.contains(i)).collect();
        for input in stale {
            host.close_device(input);
        }
        for input in &wanted {
            let Some(i) = project.inputs.get(input) else { continue };
            if i.kind == crate::devices::DEVICE_KIND {
                // an interface: the engine opens it itself, every channel
                let device = i.settings["device"].as_str().unwrap_or("");
                if let Err(e) = host.open_device(*input, device) {
                    tracing::warn!(%e, device, "couldn't open the audio input");
                }
            } else if let Some(src) = self.inputs.get(input) {
                host.capture(*input, src);
            }
        }
        host.set(crate::audio::spec(project));
        crate::plugins::retain(&crate::plugins::inserts(project));
        let mut cells: Vec<_> = project
            .tracks
            .iter()
            .filter_map(|t| meters().get(t.id).map(|cell| (crate::audio::node(t.id), cell)))
            .collect();
        // buses, mixes and the master show in the mixer too
        for id in project.audio.buses.iter().map(|b| b.id).chain(project.audio.mixes.iter().map(|m| m.id)) {
            let cell = meters().get(id).unwrap_or_else(|| meters().insert(id));
            cells.push((crate::audio::node(id), cell));
        }
        let master = meters().get(lightair_core::MASTER_ID).unwrap_or_else(|| meters().insert(lightair_core::MASTER_ID));
        cells.push((lightair_audio::PROGRAM, master));
        host.set_meter_cells(cells);
    }

    /// Starts recording multitrack files into `folder`, named "<base> - <strip>.wav": the
    /// tracks and buses marked Record (every track when none is), and the program mix when
    /// `program` (audio-only recordings). Returns the files.
    /// Sets the audio engine's block size (frames): smaller is lower latency, larger a lighter
    /// load. Restarts the engine if it's running (not while multitrack files record: false).
    pub fn set_audio_block(&mut self, frames: usize, project: &Project) -> bool {
        if self.audio_block == Some(frames) {
            return true;
        }
        if crate::audio_host::STEMS_RUNNING.load(std::sync::atomic::Ordering::Relaxed) {
            return false;
        }
        self.audio_block = Some(frames);
        if self.audio.as_ref().is_some_and(|a| a.block() != frames) {
            self.audio = None;
            self.sync_audio(project);
        }
        true
    }

    /// Plays the mixer again (a plugin insert finished loading: [`crate::plugins::changed`]).
    pub fn refresh_audio(&mut self, project: &Project) {
        self.sync_audio(project);
    }

    pub fn start_stems(
        &mut self,
        project: &Project,
        folder: &std::path::Path,
        base: &str,
        program: bool,
    ) -> Result<Vec<std::path::PathBuf>, String> {
        let settings = &project.outputs.recording;
        let host = self.audio.as_mut().ok_or("the audio engine isn't running")?;
        let tap = match settings.stem_point {
            lightair_core::StemPoint::Raw => lightair_audio::Tap::Input,
            lightair_core::StemPoint::PreFader => lightair_audio::Tap::PreFader,
            lightair_core::StemPoint::PostFader => lightair_audio::Tap::PostFader,
        };
        let float = settings.stem_format == lightair_core::StemFormat::Float32;
        let marked = project.tracks.iter().any(|t| t.record) || project.audio.buses.iter().any(|b| b.record);
        let mut strips: Vec<(lightair_audio::NodeId, String, u16)> = project
            .tracks
            .iter()
            .filter(|t| t.record || !marked)
            .map(|t| {
                // a mono channel records as mono (before the pan it's the same on both sides)
                let ch = if matches!(t.channels, lightair_core::TrackChannels::Mono(_)) && tap != lightair_audio::Tap::PostFader {
                    1
                } else {
                    2
                };
                (crate::audio::node(t.id), t.name.clone(), ch)
            })
            .collect();
        strips.extend(project.audio.buses.iter().filter(|b| b.record).map(|b| (crate::audio::node(b.id), b.name.clone(), 2)));
        if program {
            strips.push((lightair_audio::PROGRAM, "Program".into(), 2));
        }
        if strips.is_empty() {
            return Err("no tracks to record".into());
        }
        std::fs::create_dir_all(folder).map_err(|e| e.to_string())?;
        let clean = |n: &str| n.chars().map(|c| if "/\\:*?\"<>|".contains(c) { '-' } else { c }).collect::<String>();
        let mut used = HashSet::new();
        let specs = strips
            .into_iter()
            .map(|(id, name, channels)| {
                let mut file = format!("{base} - {}.wav", clean(&name));
                let mut n = 2;
                while !used.insert(file.clone()) {
                    file = format!("{base} - {} {n}.wav", clean(&name));
                    n += 1;
                }
                crate::audio_host::StemSpec { id, path: folder.join(file), channels, tap, float }
            })
            .collect::<Vec<_>>();
        let files = specs.iter().map(|s| s.path.clone()).collect();
        host.start_stems(specs)?;
        Ok(files)
    }

    /// Monitoring straight from the audio engine to an output device (`None`: libobs's own,
    /// slower monitoring of the cue). `program`: hear the stream mix rather than the cue.
    pub fn set_monitor(&mut self, device: Option<String>, program: bool) {
        self.monitor = device.map(|d| (if d == "default" { String::new() } else { d }, program));
        if let Some(host) = self.audio.as_mut() {
            let m = self.monitor.clone().map(|(d, p)| (d, if p { lightair_audio::PROGRAM } else { crate::audio::CUE }));
            if let Err(e) = host.set_monitor(m) {
                tracing::warn!(%e, "direct monitoring unavailable; libobs monitors the cue");
            }
        }
    }

    /// A monitor the caller reads (tests): stereo, interleaved.
    pub fn audio_test_monitor(&mut self, program: bool) -> Option<rtrb::Consumer<f32>> {
        let src = if program { lightair_audio::PROGRAM } else { crate::audio::CUE };
        Some(self.audio.as_mut()?.test_monitor(src))
    }

    /// An input the caller feeds (tests): `channels` interleaved frames, like an interface.
    pub fn audio_test_input(&mut self, input: Id, channels: u16) -> Option<rtrb::Producer<f32>> {
        Some(self.audio.as_mut()?.test_input(input, channels))
    }

    /// Stops the multitrack files; what they hold.
    pub fn stop_stems(&mut self) -> Option<crate::audio_host::StemReport> {
        self.audio.as_mut()?.stop_stems()
    }

    pub fn pause_stems(&mut self, pause: bool) {
        if let Some(h) = self.audio.as_mut() {
            h.pause_stems(pause);
        }
    }

    /// A track's, bus's, mix's or the program's meter from the audio engine.
    pub fn audio_meter(&mut self, id: Id) -> Option<Arc<lightair_audio::meter::Meter>> {
        Some(self.audio.as_mut()?.meter(crate::audio::node(id)))
    }

    /// The program's (master's) meter.
    pub fn program_meter(&mut self) -> Option<Arc<lightair_audio::meter::Meter>> {
        Some(self.audio.as_mut()?.meter(lightair_audio::PROGRAM))
    }

    // ------------------------------------------------------------ formats
    fn add_format(&mut self, project: &Project, format: &Format) {
        if self.formats.contains(&format.id) {
            return;
        }
        if Some(format.id) != self.primary_format {
            match Canvas::create(&format.id.to_string(), format.width, format.height, format.fps_num, format.fps_den) {
                Some(canvas) => {
                    self.canvases.insert(format.id, canvas);
                }
                None => {
                    tracing::error!(format = format.name, "could not create a canvas");
                    return;
                }
            }
        }
        self.formats.push(format.id);
        if let Some(tr) = Source::create_private("fade_transition", &format!("transition:{}", format.id), None) {
            match self.canvases.get(&format.id) {
                Some(canvas) => unsafe { sys::obs_canvas_set_channel(canvas.as_ptr(), 0, tr.as_ptr()) },
                None => unsafe { sys::obs_set_output_source(0, tr.as_ptr()) },
            }
            self.transitions.insert(format.id, tr);
        }
        // Scenes that already exist get their version in the new format; scenes added later in
        // the same patch list arrive through SceneAdded.
        for scene in &project.scenes {
            if self.scenes.keys().any(|(s, _)| *s == scene.id) {
                self.add_scene_in(&scene.flatten(), format.id);
            }
        }
        if let Some(program) = self.program {
            self.land(Some(program));
        }
    }

    fn resize_format(&mut self, format: &Format) {
        if Some(format.id) == self.primary_format {
            // The main video can be reset while nothing records or streams from it.
            unsafe {
                let mut ovi = std::mem::zeroed::<sys::obs_video_info>();
                if !sys::obs_get_video_info(&mut ovi) {
                    return;
                }
                // Not read once graphics are up, but must point at something.
                ovi.graphics_module = c"libobs-opengl".as_ptr();
                ovi.base_width = format.width;
                ovi.base_height = format.height;
                ovi.output_width = format.width;
                ovi.output_height = format.height;
                ovi.fps_num = format.fps_num;
                ovi.fps_den = format.fps_den.max(1);
                let code = sys::obs_reset_video(&mut ovi);
                if code != 0 {
                    tracing::warn!(code, "couldn't resize the main canvas (is something recording or live?)");
                }
            }
        } else if let Some(canvas) = self.canvases.get(&format.id) {
            canvas.reset(format.width, format.height, format.fps_num, format.fps_den);
        }
    }

    fn remove_format(&mut self, id: Id) {
        self.finish_move();
        self.transitions.remove(&id);
        self.items.retain(|(_, f, _), _| *f != id);
        self.masks.retain(|(_, f, _), _| *f != id);
        self.scenes.retain(|(_, f), _| *f != id);
        self.canvases.remove(&id);
        self.formats.retain(|f| *f != id);
    }

    fn add_scene_in(&mut self, scene: &lightair_core::Scene, format: Id) {
        if self.scenes.contains_key(&(scene.id, format)) {
            return;
        }
        let name = format!("{}:{}", scene.id, format);
        let obs_scene = match self.canvases.get(&format) {
            Some(canvas) => canvas.create_scene(&name),
            None => Scene::create(&name),
        };
        let Some(obs_scene) = obs_scene else { return };
        self.scenes.insert((scene.id, format), obs_scene);
        for (index, layer) in scene.layers.iter().enumerate() {
            self.add_layer(scene.id, format, layer, index);
        }
    }

    /// The canvas behind a format, as a new strong reference (for previews).
    /// The libobs source of an input (borrowed; valid while the input exists).
    pub(crate) fn input_ptr(&self, input: Id) -> Option<*mut sys::obs_source_t> {
        self.inputs.get(&input).map(|s| s.as_ptr())
    }

    /// Shows a layer whole while it's being cropped (crop mode), or `None` to end it. The
    /// document is untouched: this is only how libobs draws it meanwhile.
    pub fn set_crop_preview(&mut self, target: Option<(Id, Id, Id)>, project: &Project) {
        // The same target again while fading out: back in (the ghost fades in again).
        if self.crop_preview == target && !self.crop_fading {
            return;
        }
        let previous = std::mem::replace(&mut self.crop_preview, target);
        self.crop_fading = false;
        for (scene, format, layer) in previous.into_iter().chain(target) {
            let Some(s) = project.scene(scene).map(|s| s.flatten()) else { continue };
            let Some(index) = s.layers.iter().position(|l| l.id == layer) else { continue };
            let l = s.layers[index].clone();
            self.update_layer(scene, format, &l, index);
        }
    }

    /// Fades the crop view's ghost out (before `set_crop_preview(None)`, so leaving crop view
    /// doesn't pop): the picture then looks just as it will once the crop is applied again.
    pub fn fade_crop_preview(&mut self, project: &Project) {
        let Some((scene, format, layer)) = self.crop_preview else { return };
        self.crop_fading = true;
        let Some(s) = project.scene(scene).map(|s| s.flatten()) else { return };
        let Some(index) = s.layers.iter().position(|l| l.id == layer) else { return };
        let l = s.layers[index].clone();
        self.update_layer(scene, format, &l, index);
    }

    /// Whether the input's face effects see a face right now (`None`: it has none running).
    pub fn face_present(&self, input: Id) -> Option<bool> {
        self.studio_query(input, c"face_present")
    }

    /// Turns one of an input's effect filters on or off in libobs only, leaving the document
    /// alone (the eyedropper shows the picture without the effect being picked for). `filter` is
    /// the lowered chain's id: the effect's own, or `catalog::STUDIO_ID` for face effects.
    pub fn set_filter_live(&self, input: Id, filter: Id, enabled: bool) {
        if let Some(chain) = self.input_effects.get(&input)
            && let Some((_, f)) = chain.iter().find(|(id, _)| *id == filter)
        {
            unsafe { sys::obs_source_set_enabled(f.as_ptr(), enabled) };
        }
    }

    /// Whether the input's Camera Studio sees a person (for the background effects' hint);
    /// `None` when it has no Camera Studio running.
    pub fn person_present(&self, input: Id) -> Option<bool> {
        self.studio_query(input, c"person_present")
    }

    /// Calls a `void name(out bool present)` proc on the input's enabled Camera Studio filter.
    fn studio_query(&self, input: Id, name: &std::ffi::CStr) -> Option<bool> {
        let chain = self.input_effects.get(&input)?;
        chain.iter().find_map(|(_, filter)| unsafe {
            let id = std::ffi::CStr::from_ptr(sys::obs_source_get_id(filter.as_ptr()));
            if id.to_bytes() != b"lightair_camera_studio" || !sys::obs_source_enabled(filter.as_ptr()) {
                return None;
            }
            let ph = sys::obs_source_get_proc_handler(filter.as_ptr());
            let mut cd = sys::calldata_t::default();
            let mut present = false;
            if sys::proc_handler_call(ph, name.as_ptr(), &mut cd) {
                // calldata_get_bool is a header inline over calldata_get_data.
                sys::calldata_get_data(
                    &cd,
                    c"present".as_ptr(),
                    &mut present as *mut bool as *mut std::ffi::c_void,
                    std::mem::size_of::<bool>(),
                );
            }
            if !cd.fixed && !cd.stack.is_null() {
                sys::bfree(cd.stack as *mut std::ffi::c_void);
            }
            Some(present)
        })
    }

    /// Starts inputs as if they were on screen (captures open, media loads) without putting
    /// them on air, until `release_inputs`.
    pub fn hold_inputs(&mut self, inputs: &[Id]) {
        for id in inputs {
            if let Some(src) = self.inputs.get(id)
                && self.held.insert(*id)
            {
                unsafe { sys::obs_source_inc_showing(src.as_ptr()) };
            }
        }
    }

    pub fn release_inputs(&mut self) {
        for id in std::mem::take(&mut self.held) {
            if let Some(src) = self.inputs.get(&id) {
                unsafe { sys::obs_source_dec_showing(src.as_ptr()) };
            }
        }
    }

    /// The choices an input offers for a list setting (a camera's devices, a display capture's
    /// displays or windows, a microphone's devices): (label, value). Read from the input itself,
    /// with its current settings applied, since some sources fill their lists only then.
    pub fn input_options(&self, input: Id, property: &str) -> Vec<(String, serde_json::Value)> {
        let mut out = Vec::new();
        let Some(src) = self.inputs.get(&input) else { return out };
        let name = std::ffi::CString::new(property).unwrap_or_default();
        unsafe {
            let props = sys::obs_source_properties(src.as_ptr());
            if props.is_null() {
                return out;
            }
            let settings = sys::obs_source_get_settings(src.as_ptr());
            sys::obs_properties_apply_settings(props, settings);
            sys::obs_data_release(settings);
            let p = sys::obs_properties_get(props, name.as_ptr());
            if !p.is_null() {
                let int = sys::obs_property_list_format(p) == sys::OBS_COMBO_FORMAT_INT;
                for i in 0..sys::obs_property_list_item_count(p) {
                    let label = sys::obs_property_list_item_name(p, i);
                    let label = if label.is_null() {
                        String::new()
                    } else {
                        std::ffi::CStr::from_ptr(label).to_string_lossy().into_owned()
                    };
                    let value = if int {
                        serde_json::Value::from(sys::obs_property_list_item_int(p, i))
                    } else {
                        let v = sys::obs_property_list_item_string(p, i);
                        if v.is_null() {
                            continue;
                        }
                        serde_json::Value::from(std::ffi::CStr::from_ptr(v).to_string_lossy().into_owned())
                    };
                    if !label.is_empty() {
                        out.push((label, value));
                    }
                }
            }
            sys::obs_properties_destroy(props);
        }
        out
    }

    /// Every input's pixel size that's known (for the editor's frames and handles).
    pub fn input_sizes(&self) -> HashMap<Id, (u32, u32)> {
        self.inputs.keys().filter_map(|id| self.input_size(*id).map(|s| (*id, s))).collect()
    }

    /// An input's pixel size, once it has one (async sources report it after their first frame).
    pub fn input_size(&self, input: Id) -> Option<(u32, u32)> {
        let src = self.inputs.get(&input)?;
        let (w, h) = unsafe { (sys::obs_source_get_width(src.as_ptr()), sys::obs_source_get_height(src.as_ptr())) };
        (w > 0 && h > 0).then_some((w, h))
    }

    /// The libobs scene source for a document scene in a format, if it exists.
    pub fn scene_source(&self, scene: Id, format: Id) -> Option<*mut sys::obs_source_t> {
        self.scenes.get(&(scene, format)).map(|s| s.source_ptr())
    }

    /// A new strong reference to that scene source (the caller releases it), e.g. for Studio
    /// mode's preview of a scene that isn't on air.
    pub fn scene_source_ref(&self, scene: Id, format: Id) -> Option<*mut sys::obs_source_t> {
        self.scene_source(scene, format).map(|s| unsafe { sys::obs_source_get_ref(s) }).filter(|p| !p.is_null())
    }

    pub fn canvas_ref(&self, format: Id) -> Option<*mut sys::obs_canvas_t> {
        if Some(format) == self.primary_format {
            return Some(unsafe { sys::obs_get_main_canvas() });
        }
        self.canvases.get(&format).map(|c| unsafe { sys::obs_canvas_get_ref(c.as_ptr()) })
    }

    /// The canvas's video output, for encoders recording or streaming a format.
    pub fn format_video(&self, format: Id) -> Option<crate::outputs::VideoOutput> {
        if Some(format) == self.primary_format {
            return Some(crate::outputs::VideoOutput(unsafe { sys::obs_get_video() }));
        }
        self.canvases
            .get(&format)
            .map(|c| crate::outputs::VideoOutput(unsafe { sys::obs_canvas_get_video(c.as_ptr()) }))
    }

    fn dest(&self, program: Option<Id>, format: Id) -> *mut sys::obs_source_t {
        program.and_then(|id| self.scenes.get(&(id, format))).map_or(std::ptr::null_mut(), |s| s.source_ptr())
    }

    /// Puts `program` on air in every format instantly.
    fn land(&mut self, program: Option<Id>) {
        for format in self.formats.clone() {
            if let Some(tr) = self.transitions.get(&format) {
                unsafe { sys::obs_transition_set(tr.as_ptr(), self.dest(program, format)) };
            }
        }
    }

    fn set_program(&mut self, program: Option<Id>, project: &Project) {
        let previous = self.program;
        self.finish_move();
        let kind = match (previous, program) {
            (Some(a), Some(b)) if a != b => self.transition_settings.kind,
            _ => TransitionKind::Cut,
        };
        let ms = self.transition_settings.duration_ms;
        match kind {
            TransitionKind::Cut => self.land(program),
            TransitionKind::Fade
            | TransitionKind::Slide
            | TransitionKind::Wipe
            | TransitionKind::Dip
            | TransitionKind::Stinger => {
                let (type_id, settings) = transition_source(&self.transition_settings, self.stinger.as_ref());
                for format in self.formats.clone() {
                    self.ensure_transition(format, type_id, &settings);
                    if let Some(tr) = self.transitions.get(&format) {
                        unsafe {
                            sys::obs_transition_start(
                                tr.as_ptr(),
                                sys::OBS_TRANSITION_MODE_AUTO,
                                ms,
                                self.dest(program, format),
                            )
                        };
                    }
                }
            }
            TransitionKind::Move => {
                let (Some(a), Some(b)) = (previous, program) else { unreachable!() };
                if !self.start_move(a, b, project) {
                    self.land(program);
                }
            }
        }
        self.program = program;
    }

    fn prepare_transitions(&mut self) {
        if matches!(self.transition_settings.kind, TransitionKind::Cut | TransitionKind::Move) {
            return;
        }
        let (type_id, settings) = transition_source(&self.transition_settings, self.stinger.as_ref());
        for format in self.formats.clone() {
            self.ensure_transition(format, type_id, &settings);
        }
    }

    /// Makes `format`'s output transition a `type_id` source with `settings`, swapping the
    /// source on the output channel when the type changes (libobs carries over what's showing).
    fn ensure_transition(&mut self, format: Id, type_id: &str, settings: &serde_json::Value) {
        let Some(current) = self.transitions.get(&format) else { return };
        let current_type = unsafe { std::ffi::CStr::from_ptr(sys::obs_source_get_id(current.as_ptr())) };
        let data = data_from_json(settings);
        if current_type.to_bytes() == type_id.as_bytes() {
            current.update(&data);
            return;
        }
        let Some(next) = Source::create_private(type_id, &format!("transition:{format}"), Some(&data)) else {
            tracing::warn!(type_id, "transition type unavailable");
            return;
        };
        unsafe {
            sys::obs_transition_swap_begin(next.as_ptr(), current.as_ptr());
            match self.canvases.get(&format) {
                Some(canvas) => sys::obs_canvas_set_channel(canvas.as_ptr(), 0, next.as_ptr()),
                None => sys::obs_set_output_source(0, next.as_ptr()),
            }
            sys::obs_transition_swap_end(next.as_ptr(), current.as_ptr());
        }
        self.transitions.insert(format, next);
    }

    /// Builds the temporary scenes for a Move from `a` to `b` and starts the animation.
    fn start_move(&mut self, a: Id, b: Id, project: &Project) -> bool {
        let (Some(scene_a), Some(scene_b)) =
            (project.scene(a).map(|s| s.flatten()), project.scene(b).map(|s| s.flatten()))
        else {
            return false;
        };
        let (scene_a, scene_b) = (&scene_a, &scene_b);
        let ms = self.transition_settings.duration_ms;
        let mut formats = Vec::new();
        for format in self.formats.clone() {
            let (Some(tr), dest) = (self.transitions.get(&format), self.dest(Some(b), format)) else { continue };
            let name = format!("move:{format}:{}", Id::new());
            let temp = match self.canvases.get(&format) {
                Some(canvas) => canvas.create_scene(&name),
                None => Scene::create(&name),
            };
            let Some(temp) = temp else { continue };
            let mut tweens = Vec::new();
            let mut fades = Vec::new();
            let mut masks = Vec::new();
            // Which of A's layers continue into B.
            let mut used: Vec<Id> = Vec::new();
            let matches: Vec<Option<Layer>> = scene_b
                .layers
                .iter()
                .map(|l| {
                    let m = match_layer(scene_a, l).filter(|m| !used.contains(&m.id)).cloned();
                    if let Some(m) = &m {
                        used.push(m.id);
                    }
                    m
                })
                .collect();
            // Layers leaving: at the bottom, fading out.
            for l in scene_a.layers.iter().filter(|l| !used.contains(&l.id)) {
                let (Some(lo), Some(input)) = (l.layouts.get(&format), l.input.and_then(|i| self.inputs.get(&i)))
                else {
                    continue;
                };
                if !lo.visible {
                    continue;
                }
                let Some((item, masked)) =
                    (unsafe { add_item(temp.as_ptr(), input, Some(lo), &format!("{}", l.id), false) })
                else {
                    continue;
                };
                unsafe { layout_item(item, masked.as_ref(), lo) };
                masks.extend(masked);
                item_fade(item, false, ms, &mut fades);
                unsafe {
                    sys::obs_sceneitem_set_visible(item, false);
                }
            }
            // B's layers: gliding from A's layout when they continue, fading in otherwise.
            for (l, m) in scene_b.layers.iter().zip(&matches) {
                let (Some(to), Some(input)) = (l.layouts.get(&format), l.input.and_then(|i| self.inputs.get(&i)))
                else {
                    continue;
                };
                if !to.visible {
                    continue;
                }
                // The shape (if any) is B's: a layer gliding into a circle is a circle on the way.
                let Some((item, masked)) =
                    (unsafe { add_item(temp.as_ptr(), input, Some(to), &format!("{}", l.id), false) })
                else {
                    continue;
                };
                match m.as_ref().and_then(|m| m.layouts.get(&format)).filter(|f| f.visible) {
                    Some(from) => {
                        // B's shape and flips throughout; tilt glides.
                        let from = Layout { shape: to.shape, flip_h: to.flip_h, flip_v: to.flip_v, ..*from };
                        unsafe { layout_item(item, masked.as_ref(), &from) };
                        tweens.push(Tween { item, masked, from, to: *to });
                    }
                    None => unsafe {
                        layout_item(item, masked.as_ref(), to);
                        masks.extend(masked);
                        sys::obs_sceneitem_set_visible(item, false);
                        item_fade(item, true, ms, &mut fades);
                        sys::obs_sceneitem_set_visible(item, true);
                    },
                }
            }
            unsafe { sys::obs_transition_set(tr.as_ptr(), temp.source_ptr()) };
            formats.push(FormatMove { temp, tweens, transition: tr.as_ptr(), dest, fades, masks });
        }
        if formats.is_empty() {
            return false;
        }
        self.ensure_tick();
        *self.anim.lock() =
            Some(MoveAnim { formats, start: std::time::Instant::now(), duration: ms as f32 / 1000.0, done: false });
        true
    }

    fn ensure_timers_tick(&mut self) {
        if !self.timers_tick {
            unsafe { sys::obs_add_tick_callback(Some(on_timers_tick), tick_param(&self.timers)) };
            self.timers_tick = true;
        }
    }

    pub fn timer_action(&self, input: Id, action: TimerAction) {
        self.timers.action(input, action);
    }

    pub fn timer_status(&self, input: Id) -> Option<TimerStatus> {
        self.timers.status(input)
    }

    /// The text a timer is showing.
    pub fn timer_text(&self, input: Id) -> Option<String> {
        self.timers.text(input)
    }

    /// Countdowns that ended since the last call (their end actions are the app's to run).
    pub fn take_ended_timers(&self) -> Vec<Id> {
        self.timers.take_ended()
    }

    fn ensure_tick(&mut self) {
        if !self.tick_registered {
            unsafe { sys::obs_add_tick_callback(Some(on_tick), Arc::as_ptr(&self.anim) as *mut _) };
            self.tick_registered = true;
        }
    }

    /// Lands a running Move on its destination immediately.
    fn finish_move(&mut self) {
        if let Some(anim) = self.anim.lock().as_mut() {
            anim.finish();
        }
        self.cleanup();
    }

    /// Releases a finished Move's temporary scenes (engine thread).
    pub fn cleanup(&mut self) {
        let mut guard = self.anim.lock();
        if guard.as_ref().is_some_and(|a| a.done) {
            guard.take();
        }
    }

    fn add_layer(&mut self, scene: Id, format: Id, layer: &Layer, index: usize) {
        let (Some(obs_scene), Some(input)) =
            (self.scenes.get(&(scene, format)), layer.input.and_then(|i| self.inputs.get(&i)))
        else {
            tracing::warn!(%scene, layer = %layer.id, "layer references a missing scene or input");
            return;
        };
        let layout = layer.layouts.get(&format);
        let key = (scene, format, layer.id);
        let filtered = self.crop_preview == Some(key);
        let Some((item, masked)) =
            (unsafe { add_item(obs_scene.as_ptr(), input, layout, &format!("{}:{format}", layer.id), filtered) })
        else {
            return;
        };
        unsafe { sys::obs_sceneitem_set_order_position(item, index as i32) };
        self.items.insert(key, ItemPtr(item));
        if let Some(m) = masked {
            self.masks.insert(key, m);
        }
        self.update_layer(scene, format, layer, index);
    }

    fn update_layer(&mut self, scene: Id, format: Id, layer: &Layer, index: usize) {
        let key = (scene, format, layer.id);
        let Some(item) = self.items.get(&key).copied() else { return };
        // Gaining or losing a shape changes the item's structure (so does crop view, which
        // needs the mask): rebuild it in place. The new item goes in before the old one comes
        // out, so no frame is drawn without the layer.
        let previewing = self.crop_preview == Some(key);
        let shaped = previewing || layer.layouts.get(&format).is_some_and(|l| l.needs_filters());
        if shaped != self.masks.contains_key(&key) {
            self.masks.remove(&key);
            self.items.remove(&key);
            self.add_layer(scene, format, layer, index);
            unsafe { sys::obs_sceneitem_remove(item.0) };
            return;
        }
        unsafe {
            sys::obs_sceneitem_set_locked(item.0, layer.locked);
            let size = previewing.then(|| layer.input.and_then(|i| self.input_size(i))).flatten();
            match layer.layouts.get(&format) {
                // Being cropped: the whole picture, where it would sit uncropped, with the shape
                // in the crop window and the rest as a ghost that fades in.
                Some(l) if size.is_some() && self.masks.contains_key(&key) => {
                    let (w, h) = size.unwrap();
                    let masked = self.masks.get(&key);
                    layout_item(item.0, masked, &l.uncropped((w as f32, h as f32)));
                    if let Some(m) = masked {
                        let outside = if self.crop_fading { 0.0 } else { CROP_GHOST };
                        m.filter.update(&data_from_json(&crop_view_settings(l, outside)));
                        sys::obs_source_set_enabled(m.filter.as_ptr(), true);
                    }
                }
                Some(l) => layout_item(item.0, self.masks.get(&key), l),
                // No layout in this format: the layer is hidden there.
                None => {
                    sys::obs_sceneitem_set_visible(item.0, false);
                }
            }
        }
    }

    fn reorder(&mut self, scene: Id, format: Id, order: &[Id]) {
        for (index, layer) in order.iter().enumerate() {
            if let Some(item) = self.items.get(&(scene, format, *layer)) {
                unsafe { sys::obs_sceneitem_set_order_position(item.0, index as i32) };
            }
        }
    }

    /// What libobs actually holds, read back from libobs (not from our maps): per scene and
    /// format, the items bottom-to-top with source, position, bounds, rotation and visibility.
    /// Used to check that incremental reconciliation and a full rebuild produce identical state.
    pub fn describe(&self) -> Vec<String> {
        unsafe extern "C" fn each(
            _: *mut sys::obs_scene_t,
            item: *mut sys::obs_sceneitem_t,
            param: *mut std::ffi::c_void,
        ) -> bool {
            let out = unsafe { &mut *(param as *mut Vec<String>) };
            unsafe {
                let src = sys::obs_sceneitem_get_source(item);
                let name = std::ffi::CStr::from_ptr(sys::obs_source_get_name(src)).to_string_lossy().into_owned();
                let mut pos = vec2(0.0, 0.0);
                let mut bounds = vec2(0.0, 0.0);
                sys::obs_sceneitem_get_pos(item, &mut pos);
                sys::obs_sceneitem_get_bounds(item, &mut bounds);
                let (p, b) = (pos.__bindgen_anon_1.__bindgen_anon_1, bounds.__bindgen_anon_1.__bindgen_anon_1);
                out.push(format!(
                    "  {name} pos=({:.1},{:.1}) bounds=({:.1},{:.1}) rot={:.1} visible={}",
                    p.x,
                    p.y,
                    b.x,
                    b.y,
                    sys::obs_sceneitem_get_rot(item),
                    sys::obs_sceneitem_visible(item)
                ));
            }
            true
        }
        let mut keys: Vec<&(Id, Id)> = self.scenes.keys().collect();
        keys.sort();
        let mut out = Vec::new();
        for key in keys {
            let (scene, format) = key;
            out.push(format!(
                "scene {scene} format {format}{}",
                if self.program == Some(*scene) { " (program)" } else { "" }
            ));
            unsafe { sys::obs_scene_enum_items(self.scenes[key].as_ptr(), Some(each), &mut out as *mut _ as *mut _) };
        }
        out
    }

    // ------------------------------------------------------------ audio tracks
    fn add_track(&mut self, track: &Track) {
        let Some(source) = self.inputs.get(&track.input) else {
            tracing::warn!(track = track.name, "track references a missing input");
            return;
        };
        let used: std::collections::HashSet<u32> = self.tracks.values().map(|t| t.channel).collect();
        let Some(channel) = (1..crate::audio_host::OUT_CHANNELS.start).find(|c| !used.contains(c)) else {
            tracing::error!("no free output channel for track");
            return;
        };
        let src = source.as_ptr();
        let meter = meters().insert(track.id);
        unsafe {
            sys::obs_set_output_source(channel, src);
            // with the engine, its post-fader meters fill the cell (sync_audio); otherwise libobs's
            let vm = if Self::engine_audio() {
                std::ptr::null_mut()
            } else {
                let vm = sys::obs_volmeter_create(sys::OBS_FADER_LOG);
                sys::obs_volmeter_attach_source(vm, src);
                sys::obs_volmeter_add_callback(vm, Some(on_levels), Arc::as_ptr(&meter) as *mut _);
                vm
            };
            let mut state =
                TrackState { input: track.input, channel, volmeter: VolmeterPtr(vm), meter, filters: vec![] };
            if Self::engine_audio() {
                // levels, mute and the engine's effects are the engine's; plugin filters stay
                state.filters = chain_build(src, &libobs_effects(&track.effects));
            } else {
                apply_track_levels(src, track);
                state.filters = chain_build(src, &track.effects);
            }
            self.tracks.insert(track.id, state);
        }
    }

    fn update_track(&mut self, before: &Track, after: &Track) {
        if before.input != after.input {
            self.remove_track(after.id);
            self.add_track(after);
            return;
        }
        let (Some(state), Some(source)) = (self.tracks.get_mut(&after.id), self.inputs.get(&after.input)) else {
            return;
        };
        let src = source.as_ptr();
        if Self::engine_audio() {
            let (b, a) = (libobs_effects(&before.effects), libobs_effects(&after.effects));
            unsafe { chain_update(src, &mut state.filters, &b, &a) };
        } else {
            unsafe { apply_track_levels(src, after) };
            unsafe { chain_update(src, &mut state.filters, &before.effects, &after.effects) };
        }
    }

    fn remove_track(&mut self, id: Id) {
        let Some(mut state) = self.tracks.remove(&id) else { return };
        unsafe {
            if !state.volmeter.0.is_null() {
                sys::obs_volmeter_remove_callback(state.volmeter.0, Some(on_levels), Arc::as_ptr(&state.meter) as *mut _);
                sys::obs_volmeter_destroy(state.volmeter.0);
            }
            sys::obs_set_output_source(state.channel, std::ptr::null_mut());
            if let Some(source) = self.inputs.get(&state.input) {
                chain_remove(source.as_ptr(), &mut state.filters);
                sys::obs_source_set_monitoring_enabled(source.as_ptr(), false);
            }
        }
        meters().remove(id);
    }

    /// Items of what a format's canvas is showing right now (through its transition), as
    /// "name x y" lines with positions of the item centres. For tests and diagnostics.
    pub fn describe_on_air(&self, format: Id) -> Vec<String> {
        unsafe extern "C" fn each(
            _: *mut sys::obs_scene_t,
            item: *mut sys::obs_sceneitem_t,
            param: *mut std::ffi::c_void,
        ) -> bool {
            let out = unsafe { &mut *(param as *mut Vec<String>) };
            unsafe {
                let src = sys::obs_sceneitem_get_source(item);
                let name = std::ffi::CStr::from_ptr(sys::obs_source_get_name(src)).to_string_lossy().into_owned();
                let mut pos = vec2(0.0, 0.0);
                sys::obs_sceneitem_get_pos(item, &mut pos);
                let p = pos.__bindgen_anon_1.__bindgen_anon_1;
                out.push(format!("{name} {:.0} {:.0}", p.x, p.y));
            }
            true
        }
        let mut out = Vec::new();
        let Some(tr) = self.transitions.get(&format) else { return out };
        unsafe {
            let active = sys::obs_transition_get_active_source(tr.as_ptr());
            if active.is_null() {
                return out;
            }
            let scene = sys::obs_scene_from_source(active);
            if !scene.is_null() {
                sys::obs_scene_enum_items(scene, Some(each), &mut out as *mut _ as *mut _);
            }
            sys::obs_source_release(active);
        }
        out
    }

    /// Number of live libobs objects (tests and diagnostics).
    pub fn counts(&self) -> (usize, usize, usize) {
        (self.inputs.len(), self.scenes.len(), self.items.len())
    }

    /// Releases everything in dependency order (scenes before the inputs they use).
    pub fn clear(&mut self) {
        self.release_inputs();
        if self.timers_tick {
            unsafe { sys::obs_remove_tick_callback(Some(on_timers_tick), tick_param(&self.timers)) };
            self.timers_tick = false;
        }
        self.timers.clear();
        self.finish_move();
        if self.tick_registered {
            unsafe { sys::obs_remove_tick_callback(Some(on_tick), Arc::as_ptr(&self.anim) as *mut _) };
            self.tick_registered = false;
        }
        let ids: Vec<Id> = self.tracks.keys().copied().collect();
        for id in ids {
            self.remove_track(id);
        }
        self.audio = None; // stops the audio thread, releases the captures
        unsafe { sys::obs_set_output_source(0, std::ptr::null_mut()) };
        for canvas in self.canvases.values() {
            unsafe { sys::obs_canvas_set_channel(canvas.as_ptr(), 0, std::ptr::null_mut()) };
        }
        self.transitions.clear();
        for (id, mut chain) in self.input_effects.drain() {
            if let Some(src) = self.inputs.get(&id) {
                unsafe { chain_remove(src.as_ptr(), &mut chain) };
            }
        }
        self.masks.clear();
        self.items.clear();
        self.scenes.clear();
        self.canvases.clear();
        self.formats.clear();
        self.inputs.clear();
        self.program = None;
        // libobs destroys sources on its own thread. Wait until it has, so whatever is built
        // next gets the same names (a name still in use gets " 2" added). The render thread
        // can hold a scene one frame longer, so wait for a few quiet rounds.
        let mut quiet = 0;
        while quiet < 2 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            if unsafe { sys::obs_wait_for_destroy_queue() } {
                quiet = 0;
            } else {
                quiet += 1;
            }
        }
    }
}

pub(crate) unsafe fn apply_layout(item: *mut sys::obs_sceneitem_t, l: &Layout) {
    unsafe {
        // Centre alignment: position is the box centre, and rotation turns around it.
        sys::obs_sceneitem_set_alignment(item, sys::OBS_ALIGN_CENTER);
        sys::obs_sceneitem_set_bounds_alignment(item, sys::OBS_ALIGN_CENTER);
        let (bounds, crop_to_bounds) = match l.fit {
            Fit::Fit => (sys::OBS_BOUNDS_SCALE_INNER, false),
            Fit::Fill => (sys::OBS_BOUNDS_SCALE_OUTER, true),
            Fit::Stretch => (sys::OBS_BOUNDS_STRETCH, false),
            Fit::Original => (sys::OBS_BOUNDS_NONE, false),
        };
        sys::obs_sceneitem_set_bounds_type(item, bounds);
        sys::obs_sceneitem_set_bounds_crop(item, crop_to_bounds);
        sys::obs_sceneitem_set_bounds(item, &vec2(l.width, l.height));
        if l.fit == Fit::Original {
            sys::obs_sceneitem_set_scale(item, &vec2(1.0, 1.0));
        }
        sys::obs_sceneitem_set_pos(item, &vec2(l.x + l.width / 2.0, l.y + l.height / 2.0));
        sys::obs_sceneitem_set_rot(item, l.rotation);
        let crop = sys::obs_sceneitem_crop {
            left: l.crop.left as i32,
            top: l.crop.top as i32,
            right: l.crop.right as i32,
            bottom: l.crop.bottom as i32,
        };
        sys::obs_sceneitem_set_crop(item, &crop);
        sys::obs_sceneitem_set_visible(item, l.visible);
    }
}

/// Shape-mask filter settings (Lightair's libobs module, `lightair_shape_mask`).
fn mask_settings(s: &lightair_core::Shape) -> serde_json::Value {
    let kind = match s.kind {
        lightair_core::ShapeKind::Rounded => 0,
        lightair_core::ShapeKind::Circle => 1,
        lightair_core::ShapeKind::Pill => 2,
    };
    serde_json::json!({
        "shape": kind,
        "radius": s.radius,
        "feather": s.feather,
        "border": s.border,
        "border_color": s.border_color,
        "window_left": 0.0,
        "window_top": 0.0,
        "window_right": 0.0,
        "window_bottom": 0.0,
        "outside": 0.0,
    })
}

/// How much of the cut-away picture shows around the crop while cropping.
const CROP_GHOST: f64 = 0.42;

/// The mask while a layer is being cropped: the whole picture is laid out, the shape (or a plain
/// box) sits in the crop window, and what's outside it fades in as a ghost.
fn crop_view_settings(l: &Layout, outside: f64) -> serde_json::Value {
    let shape = l.shape.unwrap_or(lightair_core::Shape {
        radius: 0.0,
        feather: 0.0,
        border: 0.0,
        ..lightair_core::Shape::new(lightair_core::ShapeKind::Rounded)
    });
    let mut v = mask_settings(&shape);
    v["window_left"] = l.crop.left.into();
    v["window_top"] = l.crop.top.into();
    v["window_right"] = l.crop.right.into();
    v["window_bottom"] = l.crop.bottom.into();
    v["outside"] = outside.into();
    v
}

/// Flip/3D filter settings (Lightair's libobs module, `lightair_transform`).
fn transform_settings(l: &Layout) -> serde_json::Value {
    serde_json::json!({"flip_h": l.flip_h, "flip_v": l.flip_v, "tilt_x": l.tilt_x, "tilt_y": l.tilt_y})
}

/// Adds `input` to `scene` as a layer's item. A shaped layer becomes a one-item group: the input
/// sits inside (carrying the crop) and the shape mask is on the group, so the shape is cut from
/// the cropped picture and belongs to this layer alone.
pub(crate) unsafe fn add_item(
    scene: *mut sys::obs_scene_t,
    input: &Source,
    layout: Option<&Layout>,
    name: &str,
    always_filtered: bool,
) -> Option<(*mut sys::obs_sceneitem_t, Option<Masked>)> {
    unsafe {
        let Some(layout) = layout.filter(|l| always_filtered || l.needs_filters()) else {
            let item = sys::obs_scene_add(scene, input.as_ptr());
            return (!item.is_null()).then_some((item, None));
        };
        let group_name = std::ffi::CString::new(format!("shape {name} {}", Id::new())).unwrap_or_default();
        let group = sys::obs_scene_add_group2(scene, group_name.as_ptr(), false);
        if group.is_null() {
            return None;
        }
        let inner = sys::obs_scene_add(sys::obs_sceneitem_group_get_scene(group), input.as_ptr());
        let shape = layout.shape.unwrap_or(lightair_core::Shape::new(lightair_core::ShapeKind::Rounded));
        let filter = Source::create_private(
            "lightair_shape_mask",
            &format!("mask {name}"),
            Some(&data_from_json(&mask_settings(&shape))),
        );
        let Some(filter) = filter else {
            tracing::warn!("shape mask unavailable (is the Lightair module loaded?)");
            sys::obs_sceneitem_remove(group);
            let item = sys::obs_scene_add(scene, input.as_ptr());
            return (!item.is_null()).then_some((item, None));
        };
        let group_source = sys::obs_sceneitem_get_source(group);
        sys::obs_source_filter_add(group_source, filter.as_ptr());
        let transform = Source::create_private(
            "lightair_transform",
            &format!("transform {name}"),
            Some(&data_from_json(&transform_settings(layout))),
        );
        if let Some(t) = &transform {
            sys::obs_source_filter_add(group_source, t.as_ptr());
        }
        Some((group, Some(Masked { inner, filter, transform })))
    }
}

/// Lays out a layer's item; for a shaped layer the crop goes inside the group (so the mask is
/// computed on the cropped picture) and the box, fit and rotation on the group.
pub(crate) unsafe fn layout_item(item: *mut sys::obs_sceneitem_t, masked: Option<&Masked>, l: &Layout) {
    unsafe {
        let Some(m) = masked else { return apply_layout(item, l) };
        sys::obs_sceneitem_set_alignment(m.inner, sys::OBS_ALIGN_LEFT | sys::OBS_ALIGN_TOP);
        sys::obs_sceneitem_set_bounds_type(m.inner, sys::OBS_BOUNDS_NONE);
        sys::obs_sceneitem_set_scale(m.inner, &vec2(1.0, 1.0));
        sys::obs_sceneitem_set_rot(m.inner, 0.0);
        sys::obs_sceneitem_set_pos(m.inner, &vec2(0.0, 0.0));
        let crop = sys::obs_sceneitem_crop {
            left: l.crop.left as i32,
            top: l.crop.top as i32,
            right: l.crop.right as i32,
            bottom: l.crop.bottom as i32,
        };
        sys::obs_sceneitem_set_crop(m.inner, &crop);
        apply_layout(item, &Layout { crop: lightair_core::Crop::default(), ..*l });
        if let Some(shape) = &l.shape {
            m.filter.update(&data_from_json(&mask_settings(shape)));
        }
        sys::obs_source_set_enabled(m.filter.as_ptr(), l.shape.is_some());
        if let Some(t) = &m.transform {
            t.update(&data_from_json(&transform_settings(l)));
            sys::obs_source_set_enabled(t.as_ptr(), l.has_transform());
        }
    }
}

/// A track's effects that libobs runs (plugin filters): the engine runs its own (`audio.*`).
fn libobs_effects(effects: &[Effect]) -> Vec<Effect> {
    effects.iter().filter(|e| !e.kind.starts_with("audio.")).cloned().collect()
}

unsafe fn apply_track_levels(src: *mut sys::obs_source_t, track: &Track) {
    unsafe {
        sys::obs_source_set_volume(src, db_to_mul(track.volume_db));
        sys::obs_source_set_muted(src, track.muted);
        let monitoring =
            if track.cue { sys::OBS_MONITORING_TYPE_MONITOR_AND_OUTPUT } else { sys::OBS_MONITORING_TYPE_NONE };
        sys::obs_source_set_monitoring_type(src, monitoring);
    }
}

type Chain = Vec<(Id, Source)>;

/// Creates libobs filters for an effect chain and attaches them to `src`, in order.
unsafe fn chain_build(src: *mut sys::obs_source_t, effects: &[Effect]) -> Chain {
    let mut chain = Chain::new();
    for effect in effects {
        let Some(filter) =
            Source::create_private(&effect.kind, &effect.id.to_string(), Some(&data_from_json(&effect.settings)))
        else {
            tracing::warn!(kind = effect.kind, "could not create effect");
            continue;
        };
        unsafe {
            sys::obs_source_set_enabled(filter.as_ptr(), effect.enabled);
            sys::obs_source_filter_add(src, filter.as_ptr());
        }
        chain.push((effect.id, filter));
    }
    chain
}

unsafe fn chain_remove(src: *mut sys::obs_source_t, chain: &mut Chain) {
    for (_, filter) in chain.drain(..) {
        unsafe { sys::obs_source_filter_remove(src, filter.as_ptr()) };
    }
}

/// Brings a chain from `before` to `after`: rebuilt when effects were added, removed or
/// reordered; otherwise updated in place. Settings are replaced, not merged: the document holds
/// the whole intended state (a look resets everything it doesn't set).
unsafe fn chain_update(src: *mut sys::obs_source_t, chain: &mut Chain, before: &[Effect], after: &[Effect]) {
    let shape = |effects: &[Effect]| effects.iter().map(|e| (e.id, e.kind.clone())).collect::<Vec<_>>();
    if shape(before) != shape(after) {
        unsafe {
            chain_remove(src, chain);
            *chain = chain_build(src, after);
        }
        return;
    }
    for (old, new) in before.iter().zip(after) {
        let Some((_, filter)) = chain.iter().find(|(id, _)| *id == new.id) else { continue };
        if old.enabled != new.enabled {
            unsafe { sys::obs_source_set_enabled(filter.as_ptr(), new.enabled) };
        }
        if old.settings != new.settings {
            filter.reset(&data_from_json(&new.settings));
        }
    }
}

/// libobs tick (video thread): advance a running Move. Never blocks: if the engine thread holds
/// the lock, this frame is skipped.
unsafe extern "C" fn on_tick(param: *mut std::ffi::c_void, _seconds: f32) {
    let anim = unsafe { &*(param as *const Mutex<Option<MoveAnim>>) };
    if let Some(mut guard) = anim.try_lock()
        && let Some(a) = guard.as_mut()
    {
        a.step();
    }
}

/// Volume meter callback (libobs audio thread): store levels, nothing else.
unsafe extern "C" fn on_levels(
    param: *mut std::ffi::c_void,
    magnitude: *const f32,
    peak: *const f32,
    _input_peak: *const f32,
) {
    let cell = unsafe { &*(param as *const MeterCell) };
    let n = sys::MAX_AUDIO_CHANNELS as usize;
    let (magnitude, peak) = unsafe { (std::slice::from_raw_parts(magnitude, n), std::slice::from_raw_parts(peak, n)) };
    cell.store(peak, magnitude);
}

/// The libobs transition type and settings for a transition (Cut and Move use a fade source:
/// Cut sets it directly, Move lands through it).
fn transition_source(
    t: &TransitionSettings,
    stinger: Option<&lightair_core::Stinger>,
) -> (&'static str, serde_json::Value) {
    match t.kind {
        // Without a stinger video, a fade.
        TransitionKind::Stinger => match stinger {
            Some(s) => (
                "obs_stinger_transition",
                serde_json::json!({
                    "path": s.path,
                    "transition_point": s.cut_ms,
                    "tp_type": 0,
                    "hw_decode": true,
                    "preload": true,
                }),
            ),
            None => ("fade_transition", serde_json::json!({})),
        },
        TransitionKind::Slide => ("slide_transition", serde_json::json!({"direction": t.direction.as_str()})),
        TransitionKind::Wipe => {
            // Gradient images shipped with the transitions module; the dark end is revealed
            // first, so `linear-h` travels right and `linear-v` travels down.
            use lightair_core::Direction::*;
            let image = if matches!(t.direction, Left | Right) { "linear-h.png" } else { "linear-v.png" };
            let invert = matches!(t.direction, Left | Up);
            ("wipe_transition", serde_json::json!({"luma_image": image, "luma_invert": invert, "luma_softness": 0.05}))
        }
        TransitionKind::Dip => {
            ("fade_to_color_transition", serde_json::json!({"color": 0xFF000000u32, "switch_point": 50}))
        }
        TransitionKind::Cut | TransitionKind::Fade | TransitionKind::Move => ("fade_transition", serde_json::json!({})),
    }
}

fn data_from_json(value: &serde_json::Value) -> Data {
    let json = CString::new(value.to_string()).unwrap_or_default();
    let ptr = unsafe { sys::obs_data_create_from_json(json.as_ptr()) };
    Data::from_raw(ptr).unwrap_or_default()
}
