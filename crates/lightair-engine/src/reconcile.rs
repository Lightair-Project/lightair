//! The reconciler: makes libobs match the Lightair document (docs/04-engine-libobs.md,
//! "Reconciler"). It consumes patches from `lightair_core::diff`, runs on the engine thread,
//! and is idempotent: applying `diff(empty, project)` to an empty engine gives the same libobs
//! state as any sequence of edits ending at `project`.
//!
//! libobs objects are named by their Lightair id, so user-facing names never collide.

use std::collections::HashMap;
use std::ffi::CString;

use libobs_sys as sys;
use lightair_core::{Fit, Id, Layer, LayerPatch, Layout, Patch, Project, diff_layers};

use crate::obs::{Data, Scene, Source, vec2};

/// libobs state owned on behalf of the document.
#[derive(Default)]
pub struct Reconciler {
    inputs: HashMap<Id, Source>,
    scenes: HashMap<Id, Scene>,
    /// Scene items are owned by their scene; we keep the pointer to update or remove them.
    items: HashMap<(Id, Id), ItemPtr>,
    primary_format: Option<Id>,
    program: Option<Id>,
}

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

    /// Number of live libobs objects (tests and diagnostics).
    pub fn counts(&self) -> (usize, usize, usize) {
        (self.inputs.len(), self.scenes.len(), self.items.len())
    }

    /// Releases everything in dependency order (scenes before the inputs they use).
    pub fn clear(&mut self) {
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

fn data_from_json(value: &serde_json::Value) -> Data {
    let json = CString::new(value.to_string()).unwrap_or_default();
    let ptr = unsafe { sys::obs_data_create_from_json(json.as_ptr()) };
    Data::from_raw(ptr).unwrap_or_default()
}
