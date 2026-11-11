//! The reconciler: makes libobs match the Lightair document (docs/04-engine-libobs.md,
//! "Reconciler"). It consumes patches from `lightair_core::diff`, runs on the engine thread,
//! and is idempotent: applying `diff(empty, project)` to an empty engine gives the same libobs
//! state as any sequence of edits ending at `project`.
//!
//! libobs objects are named by their Lightair id, so user-facing names never collide.

use std::collections::{HashMap, HashSet};
use std::ffi::CString;

use libobs_sys as sys;
use lightair_core::{
    Effect, Fit, Format, Id, Layer, LayerPatch, Layout, Patch, Project, Track, TransitionKind, TransitionSettings,
    diff_layers,
};
use parking_lot::Mutex;

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
    formats: Vec<Id>,
    primary_format: Option<Id>,
    program: Option<Id>,
    tracks: HashMap<Id, TrackState>,
    /// Each format's output channel goes through its own transition.
    transitions: HashMap<Id, Source>,
    transition_settings: TransitionSettings,
    /// The running Move, driven by a libobs tick callback.
    anim: Arc<Mutex<Option<MoveAnim>>>,
    tick_registered: bool,
    /// Inputs held "showing" off air (e.g. to learn their size), released with `release_inputs`.
    held: HashSet<Id>,
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

impl Reconciler {
    /// Applies patches computed from two snapshots; `project` is the newer one.
    pub fn apply(&mut self, project: &Project, patches: &[Patch]) {
        self.primary_format = Some(project.primary_format().id);
        self.transition_settings = project.transition;
        self.cleanup();
        // Formats are synced from the snapshot itself, so a rebuild against `empty_like()`
        // (which keeps the formats) still creates every canvas.
        for format in &project.formats {
            if !self.formats.contains(&format.id) {
                self.add_format(project, format);
            }
        }
        for id in self.formats.clone() {
            if project.format(id).is_none() {
                self.remove_format(id);
            }
        }
        for patch in patches {
            match patch {
                Patch::FormatAdded(_) | Patch::FormatRemoved(_) => {}
                Patch::FormatChanged(format) => self.resize_format(format),
                Patch::InputAdded(input) => {
                    let settings = data_from_json(&input.settings);
                    match Source::create(&input.kind, &input.id.to_string(), Some(&settings)) {
                        Some(src) => {
                            let chain = unsafe { chain_build(src.as_ptr(), &input.effects) };
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
                        }
                        let chain = self.input_effects.entry(after.id).or_default();
                        unsafe { chain_update(src.as_ptr(), chain, &before.effects, &after.effects) };
                    }
                }
                Patch::InputRemoved(id) => {
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
                Patch::SceneAdded(scene) => {
                    for format in self.formats.clone() {
                        self.add_scene_in(scene, format);
                    }
                }
                Patch::SceneChanged { before, after } => {
                    for format in self.formats.clone() {
                        for lp in diff_layers(before, after) {
                            match lp {
                                LayerPatch::Added { layer, index } => self.add_layer(after.id, format, layer, index),
                                LayerPatch::Removed { layer } => {
                                    if let Some(item) = self.items.remove(&(after.id, format, layer)) {
                                        unsafe { sys::obs_sceneitem_remove(item.0) };
                                    }
                                }
                                LayerPatch::Changed { layer } => self.update_layer(after.id, format, layer),
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
                    self.scenes.retain(|(scene, _), _| scene != id);
                }
                Patch::ProgramChanged(program) => self.set_program(*program, project),
                Patch::TrackAdded(track) => self.add_track(track),
                Patch::TrackChanged { before, after } => self.update_track(before, after),
                Patch::TrackRemoved(id) => self.remove_track(*id),
                Patch::TrackOrder(_) => {}
            }
        }
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
                self.add_scene_in(scene, format.id);
            }
        }
        if let Some(program) = self.program {
            self.land(Some(program));
        }
    }

    fn resize_format(&mut self, format: &Format) {
        if Some(format.id) == self.primary_format {
            tracing::warn!("changing the primary canvas size needs an engine restart");
        } else if let Some(canvas) = self.canvases.get(&format.id) {
            canvas.reset(format.width, format.height, format.fps_num, format.fps_den);
        }
    }

    fn remove_format(&mut self, id: Id) {
        self.finish_move();
        self.transitions.remove(&id);
        self.items.retain(|(_, f, _), _| *f != id);
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
            TransitionKind::Fade | TransitionKind::Slide | TransitionKind::Wipe | TransitionKind::Dip => {
                let (type_id, settings) = transition_source(&self.transition_settings);
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
        let (Some(scene_a), Some(scene_b)) = (project.scene(a), project.scene(b)) else { return false };
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
                let (Some(lo), Some(input)) = (l.layouts.get(&format), self.inputs.get(&l.input)) else { continue };
                if !lo.visible {
                    continue;
                }
                let item = temp.add(input);
                if item.is_null() {
                    continue;
                }
                unsafe { apply_layout(item, lo) };
                item_fade(item, false, ms, &mut fades);
                unsafe {
                    sys::obs_sceneitem_set_visible(item, false);
                }
            }
            // B's layers: gliding from A's layout when they continue, fading in otherwise.
            for (l, m) in scene_b.layers.iter().zip(&matches) {
                let (Some(to), Some(input)) = (l.layouts.get(&format), self.inputs.get(&l.input)) else { continue };
                if !to.visible {
                    continue;
                }
                let item = temp.add(input);
                if item.is_null() {
                    continue;
                }
                match m.as_ref().and_then(|m| m.layouts.get(&format)).filter(|f| f.visible) {
                    Some(from) => {
                        unsafe { apply_layout(item, from) };
                        tweens.push(Tween { item, from: *from, to: *to });
                    }
                    None => unsafe {
                        apply_layout(item, to);
                        sys::obs_sceneitem_set_visible(item, false);
                        item_fade(item, true, ms, &mut fades);
                        sys::obs_sceneitem_set_visible(item, true);
                    },
                }
            }
            unsafe { sys::obs_transition_set(tr.as_ptr(), temp.source_ptr()) };
            formats.push(FormatMove { temp, tweens, transition: tr.as_ptr(), dest, fades });
        }
        if formats.is_empty() {
            return false;
        }
        self.ensure_tick();
        *self.anim.lock() =
            Some(MoveAnim { formats, start: std::time::Instant::now(), duration: ms as f32 / 1000.0, done: false });
        true
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
        let (Some(obs_scene), Some(input)) = (self.scenes.get(&(scene, format)), self.inputs.get(&layer.input)) else {
            tracing::warn!(%scene, layer = %layer.id, "layer references a missing scene or input");
            return;
        };
        let item = obs_scene.add(input);
        if item.is_null() {
            return;
        }
        unsafe { sys::obs_sceneitem_set_order_position(item, index as i32) };
        self.items.insert((scene, format, layer.id), ItemPtr(item));
        self.update_layer(scene, format, layer);
    }

    fn update_layer(&mut self, scene: Id, format: Id, layer: &Layer) {
        let Some(item) = self.items.get(&(scene, format, layer.id)).copied() else { return };
        unsafe {
            sys::obs_sceneitem_set_locked(item.0, layer.locked);
            match layer.layouts.get(&format) {
                Some(l) => apply_layout(item.0, l),
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
        let Some(channel) = (1..sys::MAX_CHANNELS).find(|c| !used.contains(c)) else {
            tracing::error!("no free output channel for track");
            return;
        };
        let src = source.as_ptr();
        let meter = meters().insert(track.id);
        unsafe {
            sys::obs_set_output_source(channel, src);
            let vm = sys::obs_volmeter_create(sys::OBS_FADER_LOG);
            sys::obs_volmeter_attach_source(vm, src);
            sys::obs_volmeter_add_callback(vm, Some(on_levels), Arc::as_ptr(&meter) as *mut _);
            let mut state =
                TrackState { input: track.input, channel, volmeter: VolmeterPtr(vm), meter, filters: vec![] };
            apply_track_levels(src, track);
            state.filters = chain_build(src, &track.effects);
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
        unsafe { apply_track_levels(src, after) };
        unsafe { chain_update(src, &mut state.filters, &before.effects, &after.effects) };
    }

    fn remove_track(&mut self, id: Id) {
        let Some(mut state) = self.tracks.remove(&id) else { return };
        unsafe {
            sys::obs_volmeter_remove_callback(state.volmeter.0, Some(on_levels), Arc::as_ptr(&state.meter) as *mut _);
            sys::obs_volmeter_destroy(state.volmeter.0);
            sys::obs_set_output_source(state.channel, std::ptr::null_mut());
            if let Some(source) = self.inputs.get(&state.input) {
                chain_remove(source.as_ptr(), &mut state.filters);
                sys::obs_source_set_monitoring_type(source.as_ptr(), sys::OBS_MONITORING_TYPE_NONE);
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
        self.finish_move();
        if self.tick_registered {
            unsafe { sys::obs_remove_tick_callback(Some(on_tick), Arc::as_ptr(&self.anim) as *mut _) };
            self.tick_registered = false;
        }
        let ids: Vec<Id> = self.tracks.keys().copied().collect();
        for id in ids {
            self.remove_track(id);
        }
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
        self.items.clear();
        self.scenes.clear();
        self.canvases.clear();
        self.formats.clear();
        self.inputs.clear();
        self.program = None;
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
fn transition_source(t: &TransitionSettings) -> (&'static str, serde_json::Value) {
    match t.kind {
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
