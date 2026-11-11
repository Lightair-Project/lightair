//! The reconciler: makes libobs match the Lightair document (docs/04-engine-libobs.md,
//! "Reconciler"). It consumes patches from `lightair_core::diff`, runs on the engine thread,
//! and is idempotent: applying `diff(empty, project)` to an empty engine gives the same libobs
//! state as any sequence of edits ending at `project`.
//!
//! libobs objects are named by their Lightair id, so user-facing names never collide.

use std::collections::HashMap;
use std::ffi::CString;

use libobs_sys as sys;
use lightair_core::{Effect, Fit, Id, Layer, LayerPatch, Layout, Patch, Project, Track, diff_layers};

use crate::meters::{MeterCell, db_to_mul, meters};
use crate::obs::{Data, Scene, Source, vec2};
use std::sync::Arc;

/// libobs state owned on behalf of the document.
#[derive(Default)]
pub struct Reconciler {
    inputs: HashMap<Id, Source>,
    scenes: HashMap<Id, Scene>,
    /// Scene items are owned by their scene; we keep the pointer to update or remove them.
    items: HashMap<(Id, Id), ItemPtr>,
    primary_format: Option<Id>,
    program: Option<Id>,
    tracks: HashMap<Id, TrackState>,
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
        for patch in patches {
            match patch {
                Patch::FormatAdded(_) | Patch::FormatChanged(_) | Patch::FormatRemoved(_) => {
                    // Additional canvases arrive with multi-format support (IMPLEMENTATION.md, M7).
                }
                Patch::InputAdded(input) => {
                    let settings = data_from_json(&input.settings);
                    match Source::create(&input.kind, &input.id.to_string(), Some(&settings)) {
                        Some(src) => {
                            self.inputs.insert(input.id, src);
                        }
                        None => tracing::warn!(kind = input.kind, name = input.name, "could not create input"),
                    }
                }
                Patch::InputChanged(input) => {
                    if let Some(src) = self.inputs.get(&input.id) {
                        src.update(&data_from_json(&input.settings));
                    }
                }
                Patch::InputRemoved(id) => {
                    self.inputs.remove(id);
                }
                Patch::SceneAdded(scene) => {
                    let Some(obs_scene) = Scene::create(&scene.id.to_string()) else { continue };
                    self.scenes.insert(scene.id, obs_scene);
                    for (index, layer) in scene.layers.iter().enumerate() {
                        self.add_layer(scene.id, layer, index);
                    }
                }
                Patch::SceneChanged { before, after } => {
                    for lp in diff_layers(before, after) {
                        match lp {
                            LayerPatch::Added { layer, index } => self.add_layer(after.id, layer, index),
                            LayerPatch::Removed { layer } => {
                                if let Some(item) = self.items.remove(&(after.id, layer)) {
                                    unsafe { sys::obs_sceneitem_remove(item.0) };
                                }
                            }
                            LayerPatch::Changed { layer } => self.update_layer(after.id, layer),
                            LayerPatch::Order(order) => self.reorder(after.id, &order),
                        }
                    }
                }
                Patch::SceneRemoved(id) => {
                    self.items.retain(|(scene, _), _| scene != id);
                    if self.program == Some(*id) {
                        unsafe { sys::obs_set_output_source(0, std::ptr::null_mut()) };
                        self.program = None;
                    }
                    self.scenes.remove(id);
                }
                Patch::ProgramChanged(program) => self.set_program(*program),
                Patch::TrackAdded(track) => self.add_track(track),
                Patch::TrackChanged { before, after } => self.update_track(before, after),
                Patch::TrackRemoved(id) => self.remove_track(*id),
                Patch::TrackOrder(_) => {}
            }
        }
    }

    fn set_program(&mut self, program: Option<Id>) {
        let source = program.and_then(|id| self.scenes.get(&id)).map_or(std::ptr::null_mut(), |s| s.source_ptr());
        unsafe { sys::obs_set_output_source(0, source) };
        self.program = program;
    }

    fn add_layer(&mut self, scene: Id, layer: &Layer, index: usize) {
        let (Some(obs_scene), Some(input)) = (self.scenes.get(&scene), self.inputs.get(&layer.input)) else {
            tracing::warn!(%scene, layer = %layer.id, "layer references a missing scene or input");
            return;
        };
        let item = obs_scene.add(input);
        if item.is_null() {
            return;
        }
        unsafe { sys::obs_sceneitem_set_order_position(item, index as i32) };
        self.items.insert((scene, layer.id), ItemPtr(item));
        self.update_layer(scene, layer);
    }

    fn update_layer(&mut self, scene: Id, layer: &Layer) {
        let Some(item) = self.items.get(&(scene, layer.id)).copied() else { return };
        let layout = self.primary_format.and_then(|f| layer.layouts.get(&f)).copied();
        unsafe {
            sys::obs_sceneitem_set_locked(item.0, layer.locked);
            match layout {
                Some(l) => apply_layout(item.0, &l),
                None => {
                    sys::obs_sceneitem_set_visible(item.0, false);
                }
            }
        }
    }

    fn reorder(&mut self, scene: Id, order: &[Id]) {
        for (index, layer) in order.iter().enumerate() {
            if let Some(item) = self.items.get(&(scene, *layer)) {
                unsafe { sys::obs_sceneitem_set_order_position(item.0, index as i32) };
            }
        }
    }

    /// What libobs actually holds, read back from libobs (not from our maps): per scene, the
    /// items bottom-to-top with source, position, bounds, rotation and visibility. Used to check
    /// that incremental reconciliation and a full rebuild produce identical state.
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
        let mut ids: Vec<&Id> = self.scenes.keys().collect();
        ids.sort();
        let mut out = Vec::new();
        for id in ids {
            out.push(format!("scene {id}{}", if self.program == Some(*id) { " (program)" } else { "" }));
            unsafe { sys::obs_scene_enum_items(self.scenes[id].as_ptr(), Some(each), &mut out as *mut _ as *mut _) };
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
            build_filters(src, &mut state, &track.effects);
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
        let shape = |effects: &[Effect]| effects.iter().map(|e| (e.id, e.kind.clone())).collect::<Vec<_>>();
        if shape(&before.effects) != shape(&after.effects) {
            unsafe {
                remove_filters(src, state);
                build_filters(src, state, &after.effects);
            }
            return;
        }
        for (old, new) in before.effects.iter().zip(&after.effects) {
            let Some((_, filter)) = state.filters.iter().find(|(id, _)| *id == new.id) else { continue };
            if old.enabled != new.enabled {
                unsafe { sys::obs_source_set_enabled(filter.as_ptr(), new.enabled) };
            }
            if old.settings != new.settings {
                filter.update(&data_from_json(&new.settings));
            }
        }
    }

    fn remove_track(&mut self, id: Id) {
        let Some(mut state) = self.tracks.remove(&id) else { return };
        unsafe {
            sys::obs_volmeter_remove_callback(state.volmeter.0, Some(on_levels), Arc::as_ptr(&state.meter) as *mut _);
            sys::obs_volmeter_destroy(state.volmeter.0);
            sys::obs_set_output_source(state.channel, std::ptr::null_mut());
            if let Some(source) = self.inputs.get(&state.input) {
                remove_filters(source.as_ptr(), &mut state);
                sys::obs_source_set_monitoring_type(source.as_ptr(), sys::OBS_MONITORING_TYPE_NONE);
            }
        }
        meters().remove(id);
    }

    /// Number of live libobs objects (tests and diagnostics).
    pub fn counts(&self) -> (usize, usize, usize) {
        (self.inputs.len(), self.scenes.len(), self.items.len())
    }

    /// Releases everything in dependency order (scenes before the inputs they use).
    pub fn clear(&mut self) {
        let ids: Vec<Id> = self.tracks.keys().copied().collect();
        for id in ids {
            self.remove_track(id);
        }
        unsafe { sys::obs_set_output_source(0, std::ptr::null_mut()) };
        self.items.clear();
        self.scenes.clear();
        self.inputs.clear();
        self.program = None;
    }
}

unsafe fn apply_layout(item: *mut sys::obs_sceneitem_t, l: &Layout) {
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

unsafe fn build_filters(src: *mut sys::obs_source_t, state: &mut TrackState, effects: &[Effect]) {
    for effect in effects {
        let Some(filter) =
            Source::create_private(&effect.kind, &effect.id.to_string(), Some(&data_from_json(&effect.settings)))
        else {
            tracing::warn!(kind = effect.kind, "could not create audio effect");
            continue;
        };
        unsafe {
            sys::obs_source_set_enabled(filter.as_ptr(), effect.enabled);
            sys::obs_source_filter_add(src, filter.as_ptr());
        }
        state.filters.push((effect.id, filter));
    }
}

unsafe fn remove_filters(src: *mut sys::obs_source_t, state: &mut TrackState) {
    for (_, filter) in state.filters.drain(..) {
        unsafe { sys::obs_source_filter_remove(src, filter.as_ptr()) };
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

fn data_from_json(value: &serde_json::Value) -> Data {
    let json = CString::new(value.to_string()).unwrap_or_default();
    let ptr = unsafe { sys::obs_data_create_from_json(json.as_ptr()) };
    Data::from_raw(ptr).unwrap_or_default()
}
